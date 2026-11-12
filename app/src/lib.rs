//! The application, independent of any platform.
//!
//! This is the layer the project's firmware analogy actually calls for and
//! did not have: the board support packages under `platform/` create a
//! window, translate events and manage a lifecycle, and everything *about
//! charts and tides and the boat* lives here, once.
//!
//! It exists because it was missing. Panning, tile refresh, the layer panel
//! and the under-keel check were written in `platform/linux` and then not
//! written in `platform/android`, so Android silently lagged three features
//! behind. Duplicated application logic in a board support package is how
//! that happens, and the fix is for there to be one copy that both boards
//! drive.
//!
//! What stays in a platform shim:
//!
//! * creating the window and the GPU surface, and surviving their loss
//! * turning native input into calls on [`App`]
//! * finding the data files, which is a command line on a desktop and an
//!   asset store on Android
//!
//! Everything else is here. The renderer is passed *in* rather than owned,
//! because on Android it is destroyed every time the activity is
//! backgrounded while this state has to outlive that.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod charts;
pub mod nmea;
pub use charts::ChartWorker;

use rutter_charts::{ChartSource, Collected, ImagerySource};
use rutter_core::chart::TileId;
use rutter_core::depth::{Draft, TideWindow};
use rutter_core::nmea::Vessel;
use rutter_core::tide::{Kind, Station};
use rutter_core::{chart, LatLon, View};
use rutter_render::{
    Action, DraftState, ImageryState, LayerEntry, Overlay, PanelState, Renderer, TideMarker,
    TideReading, TideState, Ui, VesselState, WindState,
};

/// Re-exported so a platform can name the panel's input without depending
/// on the renderer directly.
pub use rutter_render::PanelState as Panel;
use rutter_tides::TideStations;

// Re-exported for the platform shims. They need a position and a viewport to
// translate input into, and reaching past this crate to `rutter-core` for
// them would make the layering a suggestion rather than a rule.
pub use nmea::Endpoint as NmeaEndpoint;
pub use rutter_core::weather::Forecast;
pub use rutter_core::{LatLon as Position, View as Viewport};

/// Roughly how far apart barbs should land on screen, pixels.
const WANTED_SPACING_PX: f64 = 110.0;

/// Positions for the wind-barb lattice, in degrees.
///
/// Multiples of a fixed spacing, so they are glued to the ground: panning
/// the view returns the same positions for the ground that is still
/// visible, which is the whole point. See [`App::wind_marks`].
///
/// `base_deg` is the forecast's own node spacing. The returned spacing is
/// that times a power of two, so the coarse levels fall exactly on model
/// nodes and the fine ones subdivide them evenly.
fn barb_lattice(view: &Viewport, base_deg: f64) -> Vec<Position> {
    /// A ceiling on how many are generated, whatever the zoom. The spacing
    /// ladder should keep the count near a hundred; this is the backstop
    /// against a viewport nobody anticipated.
    const MAX_BARBS: usize = 800;
    /// How far the subdivision may go either way from the model's own
    /// spacing.
    ///
    /// Generous at the fine end on purpose. Stopping at eight times finer
    /// seemed principled — beyond that the barbs are interpolated rather
    /// than sampled — but it meant the layer vanished at harbour zoom,
    /// which is where the navigating happens. And the objection does not
    /// hold: the field is bilinear, so a subdivided barb is the same
    /// surface read more densely, not detail conjured out of nothing.
    /// Twenty near-identical barbs across a harbour are a true statement
    /// about a synoptic wind — it really is uniform at that scale.
    const FINEST: f64 = -12.0;
    const COARSEST: f64 = 8.0;

    // Spelled out rather than `!(base_deg > 0.0)` so the NaN case is a
    // decision and not a side effect of negating a partial comparison. A
    // malformed forecast file could carry a zero or a NaN step, and the
    // arithmetic below divides by it.
    if !base_deg.is_finite() || base_deg <= 0.0 {
        return Vec::new();
    }

    // Degrees of longitude per pixel. In Web Mercator a degree of longitude
    // is a constant number of pixels at any latitude, which is what makes
    // this a single number rather than a function of where you are.
    let per_pixel = 360.0 / (256.0 * 2f64.powf(view.zoom));
    let wanted = WANTED_SPACING_PX * per_pixel;

    let steps = (wanted / base_deg).log2().round().clamp(FINEST, COARSEST);
    let spacing = base_deg * 2f64.powf(steps);

    let (nw, se) = view.bounds();
    let mut out = Vec::new();

    // Indexed and multiplied, not accumulated. Repeatedly adding the
    // spacing drifts off the lattice after a few dozen steps, and "every
    // position is an exact multiple" is the property that makes these
    // stable under panning — so it is computed that way rather than
    // approached.
    let first = |edge: f64| (edge / spacing).ceil() as i64;
    let last = |edge: f64| (edge / spacing).floor() as i64;

    for row in first(se.lat)..=last(nw.lat) {
        for column in first(nw.lon)..=last(se.lon) {
            out.push(Position::new(row as f64 * spacing, column as f64 * spacing));
            if out.len() >= MAX_BARBS {
                return out;
            }
        }
    }
    out
}

/// Colours for the chart's notation, kept beside each other because they
/// are read together.
///
/// Matching the panel's own tide colour, so a ring on the chart and a high
/// water in the panel are visibly the same subject.
const TIDE_COLOUR: [f32; 4] = [0.42, 0.78, 1.0, 1.0];
const WIND_COLOUR: [f32; 4] = [0.78, 0.90, 0.94, 0.85];
/// Grey, for a forecast old enough to distrust.
const WIND_STALE_COLOUR: [f32; 4] = [0.54, 0.54, 0.54, 0.75];
/// Past this a forecast is called stale, in the panel and in the drawing.
const STALE_FORECAST_SECS: i64 = 6 * 3600;

/// The boat's own mark.
///
/// Deliberately not the colour of anything else on the chart: this is the
/// one symbol that must be findable instantly among a thousand soundings.
const BOAT_COLOUR: [f32; 4] = [1.0, 0.35, 0.1, 1.0];
/// The same mark, with a fix too old to steer by.
const BOAT_STALE_COLOUR: [f32; 4] = [0.65, 0.42, 0.33, 0.9];

/// How old a fix may be before it is shown as stale.
///
/// A GPS sends position once or twice a second, so eight seconds is several
/// missed sentences rather than a hiccup. At six knots the boat has moved
/// 25 m by then — enough to matter in a channel, which is the whole reason
/// to say so rather than to draw the old position as though it were current.
const FIX_STALE_SECS: i64 = 8;

/// How old a fix may be before it stops steering the chart.
///
/// Longer than [`FIX_STALE_SECS`]: following a stale position is still
/// better than the chart wandering off, but following a minute-old one is
/// just wrong.
const FOLLOW_STALE_SECS: i64 = 30;

/// Above this the boat is making way and its course over ground means
/// something. Below it, GPS course is noise.
const UNDERWAY_KN: f64 = 0.5;

/// The data files a platform found, however it found them.
#[derive(Debug, Default)]
pub struct Sources {
    pub charts: Option<ChartSource>,
    pub imagery: Option<ImagerySource>,
    pub tides: Option<TideStations>,
    pub weather: Option<Forecast>,
    /// Where to read NMEA 0183 from, if anywhere. Parsed by the platform
    /// from its own command line, because that is the layer that has one.
    pub nmea: Option<nmea::Endpoint>,
}

/// Tide readings for one second, reused across that second's frames.
#[derive(Debug, Clone)]
struct TideCache {
    /// The Unix second these were computed for.
    second: i64,
    /// Which station they are for, so selecting another invalidates them.
    station_id: String,
    name: String,
    now: String,
    next: Vec<TideReading>,
    curve: Vec<f32>,
    now_at: f32,
    window: TideWindow,
}

#[derive(Debug, Default)]
pub struct App {
    pub view: View,

    /// The chart, being decoded on a thread of its own. See [`charts`].
    charts: Option<ChartWorker>,
    /// The layer selection. Empty means every layer.
    ///
    /// Held here rather than on the source, which now lives on the worker:
    /// the panel needs to read it and it travels with each request.
    layers: Vec<String>,
    /// The tile set currently on the GPU.
    loaded: Vec<TileId>,
    /// The tile set last asked of the worker, which may not have arrived.
    ///
    /// Separate from `loaded` so that a viewport is asked for once and not
    /// again every frame while the answer is in flight.
    requested: Vec<TileId>,
    /// The last answer, kept so that changing the draft can re-colour the
    /// chart without decoding it again.
    last: Option<Box<Collected>>,

    tides: Option<TideStations>,
    /// Every station, loaded once.
    ///
    /// Held in memory rather than queried per frame. `nearest` and `in_box`
    /// are a SQL query each plus a second query per station for its
    /// constituents, and the panel wanted all of that three times a frame —
    /// about 1 ms of SQLite in the render path for eleven rows that never
    /// change. A few dozen stations with thirty numbers apiece is nothing
    /// to keep resident.
    stations: Vec<Station>,
    tide_visible: bool,
    /// The station being predicted for. `None` means "whichever is nearest
    /// to the middle of the screen"; a tap on a marker pins it.
    tide_pinned: Option<String>,

    /// Areas in the last collected view that carry no fill.
    unfilled: usize,
    draft: Draft,
    draft_check: bool,

    imagery: Option<ImagerySource>,
    imagery_visible: bool,
    imagery_loaded: Vec<TileId>,

    /// The wind and temperature forecast, if one was fetched.
    weather: Option<Forecast>,
    wind_visible: bool,

    /// The GPS and instrument feed, being read on a thread of its own.
    nmea: Option<nmea::Receiver>,
    /// Why there is no feed, when the reason was a bad address typed into
    /// the panel. Worth keeping: an address that will not parse is the
    /// most likely failure and the least self-explanatory.
    nmea_trouble: Option<String>,
    vessel_visible: bool,
    /// Whether the chart keeps itself centred on the boat.
    ///
    /// Off by default even with a feed: opening a chart that immediately
    /// jumps somewhere else is startling, and the boat is one click away.
    follow: bool,

    /// Last computed tide readings, and the second they were computed for.
    ///
    /// The curve and the high/low list cost about 0.6 ms together, which is
    /// worth having once a second and not sixty times. Markers are *not*
    /// cached: they move with the view and are only a projection each.
    tide_cache: Option<TideCache>,

    /// The chart's notation, as last uploaded. Compared each frame so an
    /// unchanged view does not re-upload it.
    overlay: Vec<Overlay>,
    /// Whether the last barb lattice reached past the forecast's edge.
    wind_partial: bool,

    /// Panel presentation state — which groups are open, the filter text.
    pub ui: Ui,
    /// What the layer panel shows. Rebuilt on change, not every frame.
    layer_entries: Vec<LayerEntry>,
}

impl App {
    /// Zoom limits for a pinch or a scroll, per gesture step.
    const ZOOM_STEP: f64 = 0.25;

    /// How far ahead to look for the low water the under-keel check warns
    /// about.
    ///
    /// Twelve hours is a little under one full semidiurnal cycle, so the
    /// window always contains either a low water or the fall towards one.
    /// Longer and the warning stops being about this trip; shorter and
    /// entering on a falling tide stops being flagged, which is the case
    /// the whole feature exists for.
    const DRAFT_WINDOW_HOURS: i64 = 12;

    pub fn new(sources: Sources, draft_m: Option<f64>) -> Self {
        let mut app = Self {
            charts: sources.charts.and_then(ChartWorker::spawn),
            imagery: sources.imagery,
            tides: sources.tides,
            weather: sources.weather,
            nmea: sources.nmea.and_then(nmea::Receiver::spawn),
            // On whenever there is a feed, for the same reason the other
            // layers are: the point of connecting one is to see the boat.
            vessel_visible: true,
            // On when a forecast is present, same as the other layers.
            wind_visible: true,
            // Both on when the data is there: the reason to fetch imagery or
            // stations is to see them.
            imagery_visible: true,
            tide_visible: true,
            draft: Draft {
                draft_m: draft_m.unwrap_or(Draft::default().draft_m),
                ..Draft::default()
            },
            // Only when asked for. Painting red over a chart is an answer to
            // a question nobody asked without giving a draft.
            draft_check: draft_m.is_some(),
            ..Self::default()
        };
        app.stations = app
            .tides
            .as_ref()
            .and_then(|db| db.all().ok())
            .unwrap_or_default();
        app.rebuild_layer_entries();
        app
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.view.width_px = width.max(1);
        self.view.height_px = height.max(1);
    }

    /// Centre on the chart's own extent, at its deepest zoom.
    ///
    /// Called on first launch so that the app opens on data rather than on
    /// empty sea next to it.
    pub fn fit_chart(&mut self) -> bool {
        let Some(source) = &self.charts else {
            return false;
        };
        let Some(b) = source.metadata().bounds else {
            return false;
        };
        self.view.centre = LatLon::new((b[1] + b[3]) / 2.0, (b[0] + b[2]) / 2.0);
        self.view.zoom = f64::from(source.metadata().max_zoom);
        true
    }

    /// Zoom by a number of steps, keeping the position under `(x, y)` fixed.
    ///
    /// Anchored rather than centred because that is what both a scroll wheel
    /// and a pinch mean: the thing under the pointer is what you are looking
    /// at, and moving it away is disorienting.
    pub fn zoom_about(&mut self, steps: f64, x: f64, y: f64) {
        let before = self.view.position_at(x, y);
        self.view.zoom =
            (self.view.zoom + steps * Self::ZOOM_STEP).clamp(View::MIN_ZOOM, View::MAX_ZOOM);
        let after = self.view.position_at(x, y);

        // Shift the centre by however much the anchor moved.
        let mut centre = chart::project(self.view.centre);
        let b = chart::project(before);
        let a = chart::project(after);
        centre.x = (centre.x + b.x - a.x).clamp(0.0, 1.0);
        centre.y = (centre.y + b.y - a.y).clamp(0.0, 1.0);
        self.view.centre = chart::unproject(centre);
    }

    /// Zoom about the middle of the viewport.
    pub fn zoom_by(&mut self, steps: f64) {
        let (x, y) = (
            f64::from(self.view.width_px) / 2.0,
            f64::from(self.view.height_px) / 2.0,
        );
        self.zoom_about(steps, x, y);
    }

    /// Bring every GPU-side thing in line with the view. Call before drawing.
    pub fn refresh(&mut self, renderer: &mut Renderer) {
        // Before anything else: a new fix can move the view, and the tile
        // set, the tide station and the overlay all follow from the view.
        self.poll_nmea();
        // Then: the chart refresh reads the tide window to colour the
        // under-keel check, so a stale cache would colour against last
        // second's tide.
        self.refresh_tide_cache();
        self.refresh_charts(renderer);
        self.refresh_imagery(renderer);
        self.refresh_overlay(renderer);
    }

    /// Take whatever the NMEA reader has produced, and follow the boat if
    /// that is what was asked for.
    ///
    /// Centring is done here rather than in the reader because panning is a
    /// view decision: the thread's job ends at "this is where the boat is".
    fn poll_nmea(&mut self) {
        let Some(nmea) = &mut self.nmea else {
            return;
        };
        nmea.poll();

        if !self.follow {
            return;
        }
        let now = unix_now();
        let Some(fix) = nmea.vessel().position else {
            return;
        };
        if now - fix.at > FOLLOW_STALE_SECS {
            return;
        }
        // Set rather than nudged: a follow that eased towards the boat
        // would lag behind it at speed, and a plotter that is behind the
        // boat is worse than one that is plainly centred on it.
        self.view.centre = fix.value;
    }

    /// The boat's position and the way it is pointing, if there is a fix.
    fn boat_mark(&self) -> Option<Overlay> {
        let vessel = self.nmea.as_ref()?.vessel();
        let fix = vessel.position?;
        let age = unix_now() - fix.at;

        let course = course_to_draw(vessel);

        Some(Overlay::Boat {
            at: chart::project(fix.value),
            course_deg: course.map(|d| d as f32),
            colour: if age > FIX_STALE_SECS {
                BOAT_STALE_COLOUR
            } else {
                BOAT_COLOUR
            },
        })
    }

    /// Rebuild the chart's notation: station rings and wind barbs.
    ///
    /// These were drawn through egui at first, which gave constant screen
    /// size for nothing — and put chart notation in a different renderer
    /// with a different palette, re-tessellated every frame. They are marks
    /// on the chart, so they are drawn by the chart; `overlay.wgsl`
    /// anchors them in the world and sizes them in pixels.
    fn refresh_overlay(&mut self, renderer: &mut Renderer) {
        let mut marks: Vec<Overlay> = Vec::new();

        // Wind under the stations: a station is a place and the wind is
        // context, so the station should not be hidden behind a barb.
        if self.wind_visible {
            let (barbs, partial) = self.wind_marks();
            self.wind_partial = partial;
            marks.extend(barbs);
        }
        if self.tide_visible {
            let selected = self.tide_station().map(|s| s.id.clone());
            let (nw, se) = self.view.bounds();
            for station in &self.stations {
                if station.lat < se.lat
                    || station.lat > nw.lat
                    || station.lon < nw.lon
                    || station.lon > se.lon
                {
                    continue;
                }
                marks.push(Overlay::Ring {
                    at: chart::project(LatLon::new(station.lat, station.lon)),
                    radius_px: 5.0,
                    emphasised: selected.as_deref() == Some(station.id.as_str()),
                    colour: TIDE_COLOUR,
                });
            }
        }

        // The boat last, so it is drawn over the notation rather than
        // under it. Of everything on this chart it is the one mark that
        // must never be hidden by another.
        if self.vessel_visible {
            marks.extend(self.boat_mark());
        }

        // Compared before uploading: a still view rebuilds the same marks
        // every frame, and re-uploading them would undo the point of
        // having them on the GPU at all.
        if marks != self.overlay {
            renderer.set_overlay(&marks);
            log::debug!(
                "overlay: {} marks, {} segments",
                marks.len(),
                renderer.overlay_segment_count()
            );
            self.overlay = marks;
        }
    }

    /// Wind barbs at fixed geographic positions.
    ///
    /// On a *geographic* lattice, not a screen one. The first version used
    /// screen positions — a barb every 150 pixels, sampling whatever wind
    /// was under it — and the result was barbs nailed to the window: panning
    /// slid the coast beneath them while they sat still and silently changed
    /// value. A barb has to belong to a place.
    ///
    /// The spacing is a power-of-two multiple of the forecast's own node
    /// spacing, chosen so the barbs land roughly [`WANTED_SPACING_PX`] apart
    /// on screen. Tying it to the model's grid rather than to a round number
    /// of degrees means every barb sits exactly on a model node at the
    /// coarsest levels, and that the lattice never shifts as you pan — the
    /// positions are multiples of a fixed spacing, not offsets from the
    /// viewport.
    ///
    /// Zooming does change which positions are drawn, because the
    /// subdivision level changes. That is the same thing a graticule or a
    /// contour interval does, and it reads as detail appearing rather than
    /// as the field moving.
    fn wind_marks(&self) -> (Vec<Overlay>, bool) {
        let Some(forecast) = &self.weather else {
            return (Vec::new(), false);
        };
        let Some((step, drift)) = forecast.nearest(unix_now()) else {
            return (Vec::new(), false);
        };
        // A stale forecast is still drawn, because it beats nothing, but it
        // is drawn apologetically.
        let colour = if drift.abs() > STALE_FORECAST_SECS {
            WIND_STALE_COLOUR
        } else {
            WIND_COLOUR
        };

        let lattice = barb_lattice(&self.view, step.wind.u.dlon);
        let kept: Vec<Overlay> = lattice
            .iter()
            .filter_map(|&at| {
                let wind = step.wind.sample(at)?;
                Some(Overlay::Barb {
                    at: chart::project(at),
                    speed_kn: wind.speed_kn() as f32,
                    from_deg: wind.direction_from_deg() as f32,
                    colour,
                })
            })
            .collect();
        let kept_count = kept.len();
        log::debug!(
            "wind: viewport {}x{} at z{:.1}, lattice {} points, {} inside the forecast",
            self.view.width_px,
            self.view.height_px,
            self.view.zoom,
            lattice.len(),
            kept.len()
        );
        (kept, kept_count < lattice.len())
    }

    /// Gather what the panel needs. See [`PanelState`].
    pub fn panel_state(&self, renderer: &Renderer, pixels_per_point: f32) -> PanelState {
        PanelState {
            layers: self.layer_entries.clone(),
            vessel: self.vessel_state(),
            imagery: ImageryState {
                available: self.imagery.is_some(),
                visible: self.imagery_visible,
                opacity: renderer.imagery_opacity(),
                tiles: renderer.imagery_tile_count(),
                attribution: self
                    .imagery
                    .as_ref()
                    .and_then(|i| i.metadata().attribution.clone()),
            },
            tides: self.tide_state(pixels_per_point),
            draft: self.draft_state(renderer),
            wind: self.wind_state(),
            status: status_line(&self.view),
        }
    }

    /// Draw the panel for one frame and report what the user did.
    pub fn show(&mut self, ui: &mut egui::Ui, state: &PanelState) -> Vec<Action> {
        self.ui.show(ui, state)
    }

    /// Forget everything uploaded to a renderer that has gone away.
    ///
    /// Android destroys the surface on every backgrounding, taking the
    /// chart buffer and every imagery texture with it. The record of what
    /// was uploaded then becomes a lie, and a stale record means a blank
    /// chart on resume that no amount of panning fixes.
    pub fn forget_uploads(&mut self) {
        self.loaded.clear();
        self.imagery_loaded.clear();
        // Also the outstanding request, or the chart would wait forever for
        // an answer whose geometry went with the old renderer.
        self.requested.clear();
    }

    /// Pan by a screen-space delta in pixels.
    ///
    /// Done through the projection rather than by adding degrees to the
    /// centre: a degree of longitude is not a fixed number of pixels, so the
    /// naive version drags at the wrong speed everywhere except the equator
    /// and drags the wrong way near the poles.
    pub fn pan_pixels(&mut self, dx: f64, dy: f64) {
        // A drag means "look over there", which cannot coexist with the
        // chart pulling itself back to the boat. Cancelled rather than
        // fought: a view that springs back is the single most irritating
        // thing a plotter can do.
        self.follow = false;

        let pixels_per_world = 256.0 * 2f64.powf(self.view.zoom);
        let mut w = chart::project(self.view.centre);
        w.x -= dx / pixels_per_world;
        w.y -= dy / pixels_per_world;

        // Clamp rather than wrap: wrapping at the antimeridian is correct
        // behaviour for a chart but it needs the renderer to draw two copies
        // of the world, which it does not yet do.
        w.x = w.x.clamp(0.0, 1.0);
        w.y = w.y.clamp(0.0, 1.0);

        self.view.centre = chart::unproject(w);
    }

    /// Reload chart geometry if the visible tile set has changed.
    ///
    /// The comparison is against the exact tile list, not the zoom or the
    /// centre: a drag of ten pixels usually keeps the same tiles visible, and
    /// re-decoding gzip and protobuf for geometry already on the GPU is the
    /// kind of waste that shows up as a warm phone.
    fn refresh_charts(&mut self, renderer: &mut Renderer) {
        // Settled before any geometry is uploaded, because the verdict is
        // baked into vertex colours rather than applied at draw time.
        //
        // The tide is rounded to centimetres first, and that rounding is
        // what stops this being expensive. The verdict is baked into vertex
        // colours, so any change to it re-uploads every vertex in view —
        // 400,000 of them on a busy chart. Comparing raw `f64` heights made
        // that happen every second, because a second of tide is always a
        // fraction of a millimetre of change. Chart datum and NOAA's own
        // predictions are published to the centimetre, so rounding there
        // loses nothing and the re-upload now happens only when a sounding
        // could actually change verdict.
        let centimetres = |m: f64| (m * 100.0).round() / 100.0;
        let check = (self.draft_check)
            .then(|| self.tide_window())
            .flatten()
            .map(|tide| rutter_render::DraftCheck {
                draft: self.draft,
                tide: TideWindow {
                    now_m: centimetres(tide.now_m),
                    lowest_m: centimetres(tide.lowest_m),
                },
            });
        let recolour = renderer.draft_check() != check;
        if recolour {
            renderer.set_draft_check(check);
        }

        let Some(worker) = self.charts.as_mut() else {
            return;
        };

        // Ask for whatever the viewport now needs, if that is not already
        // what was asked for. The tile list is cheap; the geometry behind it
        // is what costs, and that happens on the worker.
        let zoom = worker.clamp_zoom(self.view.tile_zoom());
        let (nw, se) = self.view.bounds();
        let wanted = chart::tiles_in_view(nw, se, zoom);
        if wanted != self.requested {
            if !worker.request(&self.view, &self.layers) {
                // The worker's thread has gone. Stop asking rather than
                // leaving the chart frozen and silent about why.
                log::error!("the chart worker has stopped; no further chart updates");
                self.charts = None;
                return;
            }
            self.requested = wanted;
        }

        // Take an answer if one has arrived. Nothing waits on this: an
        // unanswered request simply means the previous geometry stays on
        // screen for another frame, which is the whole point.
        if let Some(got) = worker.take_answer() {
            self.apply_collected(renderer, &got);
            self.last = Some(got);
        } else if recolour {
            // The draft changed but the chart did not. Re-upload what is
            // already here rather than making the worker decode it again —
            // only the colours differ.
            if let Some(last) = self.last.take() {
                self.apply_collected(renderer, &last);
                self.last = Some(last);
            }
        }
    }

    /// Upload one answer and report what it contained.
    fn apply_collected(&mut self, renderer: &mut Renderer, got: &Collected) {
        if let Some(count) = got.over_budget {
            // Zoomed far outside the chart's range. Clear rather than leave
            // the previous zoom's geometry on screen pretending to be this
            // one.
            log::debug!("charts: {count} tiles wanted, over budget — not drawing");
            renderer.clear_chart();
            self.unfilled = 0;
            self.loaded = got.tiles.clone();
            return;
        }

        // One unreadable tile should not take the chart down: a corrupt blob
        // in the middle of a file still leaves the rest navigable.
        for (id, why) in &got.failed {
            log::warn!("{id:?}: {why}");
        }

        renderer.set_chart(&got.areas, &got.lines, &got.points);

        log::debug!(
            "charts: {} tiles, {} polylines, {} points, {} fill triangles, {} vertices, styles {:?}",
            got.tiles.len(),
            got.lines.len(),
            got.points.len(),
            renderer.fill_triangle_count(),
            renderer.chart_vertex_count(),
            got.styles()
        );

        // Logged separately from the geometry because it is the answer to a
        // navigational question rather than a rendering statistic, and
        // because a verdict of "nothing judged" with the check switched on
        // is a bug worth seeing rather than a quiet blank chart.
        if self.draft_check {
            let tally = renderer.clearance_tally();
            log::debug!(
                "under-keel ({:.2} m needed): {} clear, {} dry out within {} h, {} foul now, of {} judged",
                self.draft.needed_m(),
                tally.clear,
                tally.tight_at_low,
                Self::DRAFT_WINDOW_HOURS,
                tally.foul,
                tally.total()
            );
            if got.unfilled > 0 {
                log::debug!("under-keel: {} areas outlined but not shaded", got.unfilled);
            }
            if tally.total() == 0 {
                log::warn!(
                    "under-keel check found nothing with a charted depth — \
                     the chart may have lost its DEPTH/DRVAL1 attributes"
                );
            }
        }

        self.unfilled = got.unfilled;
        self.loaded = got.tiles.clone();
    }

    /// Change the layer selection. Returns true if it actually changed.
    ///
    /// The selection lives here rather than on the source, which is on the
    /// worker thread. It travels with each request, and a change forces the
    /// next one by clearing what was last asked for — the tile set has not
    /// moved, only what should be drawn from it.
    fn set_layers(&mut self, layers: &[String]) -> bool {
        if self.layers == layers {
            return false;
        }
        self.layers = layers.to_vec();
        self.requested.clear();
        true
    }

    /// Show or hide one layer by name.
    ///
    /// An empty selection means "all layers", so switching one off from
    /// that state has to expand to the full list first.
    fn toggle_layer(&mut self, name: &str, visible: bool) -> bool {
        let mut selection = if self.layers.is_empty() {
            self.chart_layer_names()
        } else {
            self.layers.clone()
        };

        if visible {
            if !selection.iter().any(|l| l == name) {
                selection.push(name.to_owned());
            }
        } else {
            selection.retain(|l| l != name);
        }
        self.set_layers(&selection)
    }

    /// Every layer the chart holds, in file order.
    fn chart_layer_names(&self) -> Vec<String> {
        self.charts
            .as_ref()
            .map(|w| w.metadata().layers.iter().map(|l| l.name.clone()).collect())
            .unwrap_or_default()
    }

    fn rebuild_layer_entries(&mut self) {
        self.layer_entries.clear();
        let Some(worker) = &self.charts else {
            return;
        };

        // An empty selection means "all visible", which is the state a
        // freshly opened chart is in.
        let selected = self.layers.clone();
        let all_visible = selected.is_empty();

        for layer in &worker.metadata().layers {
            self.layer_entries.push(LayerEntry {
                style: rutter_charts::style_for_layer(&layer.name),
                visible: all_visible || selected.contains(&layer.name),
                name: layer.name.clone(),
                count: None,
            });
        }
    }

    /// Apply what the panel reported. Returns true if the chart needs
    /// redrawing.
    pub fn apply(&mut self, actions: &[Action], renderer: &mut Renderer) -> bool {
        if actions.is_empty() {
            return false;
        }
        // Imagery lives on the renderer, not the chart source, so those
        // actions are handled before the source is borrowed below.
        let mut touched = false;
        for action in actions {
            match action {
                Action::SetImageryOpacity(opacity) => {
                    renderer.set_imagery_opacity(*opacity);
                    touched = true;
                }
                Action::SetImagery(on) => {
                    self.imagery_visible = *on;
                    // The tile set is unchanged — only whether it is drawn
                    // — so refresh_imagery needs nudging.
                    self.imagery_loaded.clear();
                    touched = true;
                }
                Action::SetWind(on) => {
                    self.wind_visible = *on;
                    touched = true;
                }
                Action::ConnectNmea(address) => {
                    match nmea::Endpoint::parse(address) {
                        // Replacing the receiver drops the old one, which
                        // is what stops its thread — see the Drop test in
                        // `nmea`. Nothing here has to join it.
                        Ok(endpoint) => {
                            self.nmea = nmea::Receiver::spawn(endpoint);
                            self.vessel_visible = true;
                        }
                        Err(e) => {
                            // Kept rather than dropped: the panel shows
                            // the reason where the link status goes, so a
                            // typo explains itself.
                            log::error!("NMEA: {address}: {e}");
                            self.nmea = None;
                            self.nmea_trouble = Some(format!("{address}: {e}"));
                        }
                    }
                    touched = true;
                }
                Action::DisconnectNmea => {
                    self.nmea = None;
                    self.nmea_trouble = None;
                    self.follow = false;
                    touched = true;
                }
                Action::SetVessel(on) => {
                    self.vessel_visible = *on;
                    // Hiding the boat stops following it too. Following
                    // something invisible is a view that moves for no
                    // reason the chart can show.
                    self.follow = self.follow && *on;
                    touched = true;
                }
                Action::SetFollow(on) => {
                    self.follow = *on;
                    if *on {
                        // Immediately, not on the next fix: a tick-box
                        // that does nothing until the GPS next speaks
                        // looks broken.
                        touched |= self.go_to_boat();
                    }
                }
                Action::GoToBoat => {
                    self.follow = false;
                    touched |= self.go_to_boat();
                }
                Action::SetTides(on) => {
                    self.tide_visible = *on;
                    touched = true;
                }
                Action::SetDraftCheck(on) => {
                    self.draft_check = *on;
                    // The verdict is baked into vertex colours, so the
                    // chart geometry has to be rebuilt rather than merely
                    // redrawn.
                    self.loaded.clear();
                    touched = true;
                }
                Action::SetDraft(draft) => {
                    self.draft = *draft;
                    self.loaded.clear();
                    touched = true;
                }
                Action::SelectTideStation(id) => {
                    // Tapping the pinned station unpins it, so there is a
                    // way back to "whichever is nearest" without a second
                    // control.
                    self.tide_pinned = if self.tide_pinned.as_deref() == Some(id.as_str()) {
                        None
                    } else {
                        Some(id.clone())
                    };
                    touched = true;
                }
                _ => {}
            }
        }

        if self.charts.is_none() {
            // Imagery and fit still work without a vector chart.
            return touched;
        }

        let mut changed = touched;
        let mut fit = false;

        for action in actions {
            match action {
                Action::SetLayer { name, visible } => {
                    changed |= self.toggle_layer(name, *visible);
                }
                Action::SetStyle { style, visible } => {
                    // Every layer in the group, one at a time. Clearer than
                    // a bulk set, and toggle_layer already handles the
                    // "empty means all" expansion.
                    let names: Vec<String> = self
                        .chart_layer_names()
                        .into_iter()
                        .filter(|name| rutter_charts::style_for_layer(name) == *style)
                        .collect();
                    for name in names {
                        changed |= self.toggle_layer(&name, *visible);
                    }
                }
                Action::ShowAll => {
                    changed |= self.set_layers(&[]);
                }
                Action::HideAll => {
                    // One name that cannot match anything. An empty list
                    // means "all", so it cannot express "none".
                    changed |= self.set_layers(&["\0none".to_owned()]);
                }
                Action::FitChart => fit = true,
                // All handled above, before the source was borrowed.
                Action::SetImagery(_)
                | Action::SetImageryOpacity(_)
                | Action::SetTides(_)
                | Action::SelectTideStation(_)
                | Action::SetDraftCheck(_)
                | Action::SetDraft(_)
                | Action::SetWind(_)
                | Action::SetVessel(_)
                | Action::SetFollow(_)
                | Action::GoToBoat
                | Action::ConnectNmea(_)
                | Action::DisconnectNmea => {}
            }
        }

        if fit {
            changed |= self.fit_chart();
        }

        if changed {
            // The tile set may be identical, so the request in
            // `refresh_charts` would be skipped; force it to ask again.
            self.requested.clear();
            self.rebuild_layer_entries();
        }
        changed
    }

    /// Centre on the boat, for a platform's own key binding.
    ///
    /// Stops following, like the panel's button does: an explicit jump is
    /// a look, not a mode.
    pub fn go_to_boat_now(&mut self) -> bool {
        self.follow = false;
        self.go_to_boat()
    }

    /// Centre on the boat. False if there is nothing to centre on.
    fn go_to_boat(&mut self) -> bool {
        let Some(fix) = self.nmea.as_ref().and_then(|n| n.vessel().position) else {
            return false;
        };
        // No staleness check here, unlike following: this was asked for
        // explicitly, and "take me to where the boat last was" is a
        // reasonable thing to want when the feed has dropped.
        self.view.centre = fix.value;
        true
    }

    /// The boat's position, if the fix is recent enough to act on.
    ///
    /// Used for choosing a tide station and nothing else so far. Separate
    /// from the marker's own staleness test because the consequences
    /// differ: a stale marker is drawn apologetically, while a stale fix
    /// picking the tide station would silently predict for the wrong
    /// harbour hours after the feed died.
    fn fresh_fix(&self) -> Option<LatLon> {
        let fix = self.nmea.as_ref()?.vessel().position?;
        (unix_now() - fix.at <= FOLLOW_STALE_SECS).then_some(fix.value)
    }

    /// What the panel shows about the boat. See [`VesselState`].
    fn vessel_state(&self) -> VesselState {
        let Some(nmea) = &self.nmea else {
            return VesselState {
                link: self.nmea_trouble.clone().unwrap_or_default(),
                trouble: self.nmea_trouble.is_some(),
                ..VesselState::default()
            };
        };
        let report = nmea.report();
        let vessel = &report.vessel;
        let now = unix_now();

        let age = vessel.position.map(|fix| now - fix.at);
        VesselState {
            available: true,
            visible: self.vessel_visible,
            link: nmea.status(),
            trouble: nmea.troubled(),
            fix: vessel.position.map(|fix| format_position(fix.value)),
            motion: motion_line(vessel),
            heading: heading_line(vessel),
            depth: vessel
                .depth_m
                .map(|d| format!("{:.1} m under the transducer", d.value)),
            wind: vessel.apparent_wind.map(|w| apparent_wind_line(w.value)),
            age: age.map(fix_age_line),
            stale: age.is_some_and(|s| s > FIX_STALE_SECS),
            quality: fix_quality_line(vessel),
            follow: self.follow,
            address: Some(format!(
                "{}://{}",
                nmea.endpoint().protocol_name().to_lowercase(),
                nmea.endpoint().address()
            )),
            counts: (report.accepted + report.unsupported + report.rejected > 0).then(|| {
                format!(
                    "{} read, {} ignored, {} refused",
                    report.accepted, report.unsupported, report.rejected
                )
            }),
        }
    }

    /// Which station to predict for: the pinned one, the one nearest the
    /// boat, or the one nearest the middle of the screen.
    ///
    /// The boat takes precedence over the view because the tide you want is
    /// the tide where you are, not the tide wherever you happened to scroll
    /// — but only while the fix is fresh, or a dead feed would pin the
    /// prediction to a harbour you left hours ago.
    fn tide_station(&self) -> Option<&Station> {
        if let Some(id) = &self.tide_pinned {
            if let Some(found) = self.stations.iter().find(|s| &s.id == id) {
                return Some(found);
            }
            // Pinned to a station that is no longer loaded — fall through to
            // the nearest rather than showing nothing.
        }
        // Linear, over a list in the dozens. Borrowed rather than cloned:
        // a station carries thirty-odd constituents and this is called
        // several times a frame.
        let from = self.fresh_fix().unwrap_or(self.view.centre);
        self.stations.iter().min_by(|a, b| {
            let da = from.distance_to(LatLon::new(a.lat, a.lon));
            let db = from.distance_to(LatLon::new(b.lat, b.lon));
            da.partial_cmp(&db).unwrap_or(core::cmp::Ordering::Equal)
        })
    }

    /// Recompute the tide readings if they are not for this second.
    ///
    /// Everything the panel shows about the tide — the curve, the next few
    /// turning points, the window the under-keel check uses — comes from
    /// here, computed once a second rather than once a frame.
    fn refresh_tide_cache(&mut self) {
        let now = unix_now();
        let Some(station) = self.tide_station() else {
            self.tide_cache = None;
            return;
        };

        if self
            .tide_cache
            .as_ref()
            .is_some_and(|c| c.second == now && c.station_id == station.id)
        {
            return;
        }

        // A window that starts in the recent past, so the curve shows where
        // the water has come from as well as where it is going. Reading a
        // tide is as much about the trend as the number.
        const BEHIND: i64 = 3 * 3600;
        const AHEAD: i64 = 21 * 3600;
        const SAMPLES: i64 = 120;
        let (from, to) = (now - BEHIND, now + AHEAD);

        let curve: Vec<f32> = station
            .curve(from, to, (to - from) / SAMPLES)
            .into_iter()
            .map(|(_, metres)| metres as f32)
            .collect();

        let next = station
            .extremes(now, to)
            .into_iter()
            .take(4)
            .map(|e| TideReading {
                label: match e.kind {
                    Kind::High => "High".to_owned(),
                    Kind::Low => "Low".to_owned(),
                },
                when: format_utc_hhmm(e.unix),
                away: format_away(e.unix - now),
                height: format!("{:.2} m", e.height_m),
                high: e.kind == Kind::High,
            })
            .collect();

        // The under-keel window, over its own shorter span.
        let ahead = now + Self::DRAFT_WINDOW_HOURS * 3600;
        let lowest = station
            .curve(now, ahead, 600)
            .into_iter()
            .map(|(_, metres)| metres)
            .fold(f64::MAX, f64::min);

        self.tide_cache = Some(TideCache {
            second: now,
            station_id: station.id.clone(),
            name: station.name.clone(),
            now: format!("{:+.2} m", station.height_at(now)),
            next,
            curve,
            now_at: BEHIND as f32 / (AHEAD + BEHIND) as f32,
            window: TideWindow {
                now_m: station.height_at(now),
                // A station with no usable curve would give f64::MAX, which
                // would make every sounding look clear. Fall back to the
                // present level, which is at worst pessimistic.
                lowest_m: if lowest.is_finite() {
                    lowest
                } else {
                    station.height_at(now)
                },
            },
        });
    }

    /// The tide now and at the lowest point of the window.
    ///
    /// The minimum is taken over a sampled curve rather than from
    /// `extremes`, because the window's lowest water may be its far end on
    /// a still-falling tide rather than a turning point inside it.
    fn tide_window(&self) -> Option<TideWindow> {
        self.tide_cache.as_ref().map(|c| c.window)
    }

    /// Everything the panel needs to show about the wind.
    fn wind_state(&self) -> WindState {
        let Some(forecast) = &self.weather else {
            return WindState::default();
        };

        let mut state = WindState {
            available: true,
            visible: self.wind_visible,
            ..WindState::default()
        };

        let Some((step, drift)) = forecast.nearest(unix_now()) else {
            return state;
        };
        state.stale = drift.abs() > STALE_FORECAST_SECS;
        state.partial_coverage = self.wind_visible && self.wind_partial;
        state.validity = Some(format!("GFS +{} h, {}", step.hour, format_away(-drift)));

        if let Some(wind) = step.wind.sample(self.view.centre) {
            let temperature = step
                .temperature
                .sample(self.view.centre)
                .map(|c| format!(", {c:.0} C"))
                .unwrap_or_default();
            state.here = Some(format!(
                "{:.0} kn from {:.0}°, force {}{}",
                wind.speed_kn(),
                wind.direction_from_deg(),
                wind.beaufort(),
                temperature
            ));
        }

        state
    }

    /// Everything the panel needs to show about the under-keel check.
    fn draft_state(&self, renderer: &Renderer) -> DraftState {
        // Needs a chart with depths *and* a tide. Either alone cannot
        // answer the question.
        let tide = self.tide_window();
        let available = self.charts.is_some() && tide.is_some();
        let tide = tide.unwrap_or(TideWindow {
            now_m: 0.0,
            lowest_m: 0.0,
        });

        DraftState {
            available,
            enabled: available && self.draft_check,
            draft: self.draft,
            tally: renderer.clearance_tally(),
            tide_now_m: tide.now_m,
            tide_low_m: tide.lowest_m,
            window_hours: Self::DRAFT_WINDOW_HOURS as u32,
            unfilled: self.unfilled,
        }
    }

    /// Everything the panel needs to show about tides.
    fn tide_state(&self, pixels_per_point: f32) -> TideState {
        let Some(db) = self.tides.as_ref() else {
            return TideState::default();
        };

        let mut state = TideState {
            available: true,
            visible: self.tide_visible,
            stations: self.stations.len(),
            attribution: db.attribution().map(str::to_owned),
            ..TideState::default()
        };

        if !self.tide_visible {
            return state;
        }

        // Markers every frame, from the in-memory list: eleven projections
        // and a box test, which is cheap enough not to cache and has to
        // follow the view anyway.
        let (nw, se) = self.view.bounds();
        let selected = self.tide_station().map(|s| s.id.as_str());
        state.markers = self
            .stations
            .iter()
            .filter(|s| s.lat >= se.lat && s.lat <= nw.lat && s.lon >= nw.lon && s.lon <= se.lon)
            .map(|s| {
                let (x, y) = self.view.screen_of(LatLon::new(s.lat, s.lon));
                TideMarker {
                    // egui works in points, the view in physical pixels.
                    x: (x / f64::from(pixels_per_point)) as f32,
                    y: (y / f64::from(pixels_per_point)) as f32,
                    id: s.id.clone(),
                    label: s.name.clone(),
                    selected: selected == Some(s.id.as_str()),
                }
            })
            .collect();

        // The readings come from the once-a-second cache.
        if let Some(cache) = &self.tide_cache {
            state.station = Some(cache.name.clone());
            state.now = Some(cache.now.clone());
            state.next = cache.next.clone();
            state.curve = cache.curve.clone();
            state.now_at = cache.now_at;
        }

        state
    }

    /// Reload imagery textures if the visible tile set has changed.
    ///
    /// Separate from `refresh_charts` because the two have different costs:
    /// a vector tile is a protobuf parse, an imagery tile is a JPEG decode
    /// and a quarter-megabyte upload.
    fn refresh_imagery(&mut self, renderer: &mut Renderer) {
        if !self.imagery_visible {
            if !self.imagery_loaded.is_empty() {
                renderer.clear_imagery();
                self.imagery_loaded.clear();
            }
            return;
        }

        let Some(source) = self.imagery.as_mut() else {
            return;
        };

        let zoom = source.clamp_zoom(self.view.tile_zoom());
        let (nw, se) = self.view.bounds();
        let wanted = chart::tiles_in_view(nw, se, zoom);
        if wanted == self.imagery_loaded {
            return;
        }

        let tiles = source.collect(&self.view);
        renderer.set_imagery(&tiles);
        log::debug!(
            "imagery: {} tiles on the GPU",
            renderer.imagery_tile_count()
        );
        self.imagery_loaded = wanted;
    }
}

/// The text in the top strip: where the view is and how far in.
///
/// Degrees and decimal minutes, which is how a position is read aloud and
/// written in a log — not decimal degrees, which no one on a boat uses.
pub fn status_line(view: &View) -> String {
    let scale = chart::metres_per_pixel(view.centre.lat, view.tile_zoom());
    format!(
        "{}   z{:.1}   {:.0} m/px",
        format_position(view.centre),
        view.zoom,
        scale
    )
}

/// Speed and course over ground, as one line.
///
/// Either half can be missing: a GGA-only receiver gives a position and
/// nothing about movement, and a VTG-only one the opposite.
fn motion_line(vessel: &Vessel) -> Option<String> {
    match (vessel.sog_kn, vessel.cog_deg) {
        (Some(sog), Some(cog)) => Some(format!("{:.1} kn   COG {:03.0}°", sog.value, cog.value)),
        (Some(sog), None) => Some(format!("{:.1} kn", sog.value)),
        (None, Some(cog)) => Some(format!("COG {:03.0}°", cog.value)),
        (None, None) => None,
    }
}

/// Compass heading, but only when it is not just the course again.
///
/// They read the same on a motorboat in still water and differ the moment
/// there is a tide on the beam — which is exactly when the difference is
/// worth a line of its own. Showing both always would mean two numbers that
/// agree, every time, until the one occasion they matter.
fn heading_line(vessel: &Vessel) -> Option<String> {
    let hdg = vessel.heading_deg?;
    let differs = vessel.cog_deg.is_none_or(|cog| {
        // Across north, 358 and 002 are four degrees apart, not 356.
        let gap = (hdg.value - cog.value).abs() % 360.0;
        gap.min(360.0 - gap) > 3.0
    });
    differs.then(|| format!("HDG {:03.0}°", hdg.value))
}

/// Apparent wind, named the way it is read on deck.
///
/// Degrees off the bow and the side it comes over, rather than the 0..360
/// the sentence carries: nobody says "the wind is at 315 relative", they
/// say "forty-five on the port bow".
fn apparent_wind_line((angle, speed): (f64, f64)) -> String {
    // Dead ahead and dead astern have no side, and claiming one would be
    // the kind of confident nonsense that makes an instrument distrusted.
    let side = if angle <= 1.0 || angle >= 359.0 || (179.0..=181.0).contains(&angle) {
        ""
    } else if angle < 180.0 {
        " starboard"
    } else {
        " port"
    };
    let off_bow = if angle > 180.0 { 360.0 - angle } else { angle };
    format!("{speed:.1} kn apparent, {off_bow:.0}°{side}")
}

/// How old the fix is, in the units it is worth hearing.
fn fix_age_line(seconds: i64) -> String {
    match seconds {
        ..=1 => "fix is current".to_owned(),
        s if s < 60 => format!("fix {s} s old"),
        s => format!("fix {} min old", s / 60),
    }
}

/// What kind of fix this is, and off how many satellites.
///
/// `None` for quality 0, which means the receiver is powered and searching.
/// There is no fix to describe, and the panel already says there is none.
fn fix_quality_line(vessel: &Vessel) -> Option<String> {
    let kind = match vessel.fix_quality {
        0 => return None,
        1 => "GPS",
        2 => "differential",
        4 => "RTK fixed",
        5 => "RTK float",
        _ => "fix",
    };
    Some(match vessel.satellites {
        Some(n) => format!("{kind}, {n} satellites"),
        None => kind.to_owned(),
    })
}

/// Which way to point the boat symbol.
///
/// Course over ground while the boat is moving, compass heading otherwise.
/// A GPS computes COG from successive positions, so at rest it is the
/// difference between two helpings of the same noise and spins freely — and
/// a boat symbol spinning on its mooring reads as a broken instrument.
fn course_to_draw(vessel: &Vessel) -> Option<f64> {
    let moving = vessel.sog_kn.is_some_and(|s| s.value >= UNDERWAY_KN);
    match (moving, vessel.cog_deg, vessel.heading_deg) {
        (true, Some(cog), _) => Some(cog.value),
        (_, _, Some(heading)) => Some(heading.value),
        _ => None,
    }
}

/// A position in degrees and decimal minutes.
///
/// Degrees and decimal minutes rather than decimal degrees, because that is
/// what a chart's margin is ruled in, what a position is given over the
/// radio in, and what NMEA itself carries. Decimal degrees would be easier
/// to print and useless to read off against a paper chart.
pub fn format_position(at: LatLon) -> String {
    let fmt = |value: f64, positive: char, negative: char| {
        let hemisphere = if value >= 0.0 { positive } else { negative };
        let magnitude = value.abs();
        let degrees = magnitude.trunc();
        let minutes = (magnitude - degrees) * 60.0;
        format!("{degrees:.0}°{minutes:05.2}'{hemisphere}")
    };
    format!("{}  {}", fmt(at.lat, 'N', 'S'), fmt(at.lon, 'E', 'W'))
}

/// Seconds since the Unix epoch, now.
///
/// Before 1970 the clock is wrong rather than the code, so a clock that
/// claims to be is clamped to the epoch instead of panicking.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `HH:MM` in UTC, plus the label.
///
/// UTC and not local time, deliberately. Navigation runs on UTC, tide tables
/// are published against a stated zone, and getting a zone wrong by an hour
/// is the kind of error that puts a boat on a bar. Doing local time properly
/// needs a timezone database, which is a dependency this has not earned yet
/// — so until then the honest thing is to say which zone this is. The
/// relative "in 2h 11m" beside it is what you actually steer by.
fn format_utc_hhmm(unix: i64) -> String {
    let seconds_into_day = unix.rem_euclid(86_400);
    format!(
        "{:02}:{:02}Z",
        seconds_into_day / 3600,
        (seconds_into_day % 3600) / 60
    )
}

/// How far off something is, as "in 2h 11m" or "12m ago".
fn format_away(seconds: i64) -> String {
    let past = seconds < 0;
    let total_minutes = seconds.abs() / 60;
    let (hours, minutes) = (total_minutes / 60, total_minutes % 60);

    let magnitude = if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        format!("{minutes}m")
    };
    if past {
        format!("{magnitude} ago")
    } else {
        format!("in {magnitude}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GFS's own spacing, which is what the lattice is built from.
    const GFS_DEG: f64 = 0.25;

    fn view_at(lat: f64, lon: f64, zoom: f64) -> Viewport {
        Viewport::new(Position::new(lat, lon), zoom, 1280, 800)
    }

    #[test]
    fn the_barb_lattice_is_glued_to_the_ground() {
        // The bug this exists for: the lattice was built on screen
        // positions, so panning slid the coast under barbs that sat still
        // and quietly changed value. Every position has to be a multiple of
        // the spacing, which makes it a property of the ground and not of
        // the window.
        let before = barb_lattice(&view_at(32.70, -117.25, 11.0), GFS_DEG);
        let after = barb_lattice(&view_at(32.70, -117.15, 11.0), GFS_DEG);

        assert!(!before.is_empty() && !after.is_empty());

        // The two views overlap, so the positions in both must be identical
        // — not merely similar in number.
        let shared: Vec<&Position> = before
            .iter()
            .filter(|p| after.iter().any(|q| same_place(p, q)))
            .collect();
        assert!(
            shared.len() > before.len() / 2,
            "a small pan should keep most positions, kept {} of {}",
            shared.len(),
            before.len()
        );

        // And the set is not the same set shifted: panning east must bring
        // in positions further east than anything in the first view.
        let easternmost_before = before.iter().map(|p| p.lon).fold(f64::MIN, f64::max);
        let easternmost_after = after.iter().map(|p| p.lon).fold(f64::MIN, f64::max);
        assert!(
            easternmost_after > easternmost_before,
            "panning east should reveal ground further east"
        );
    }

    fn same_place(a: &Position, b: &Position) -> bool {
        (a.lat - b.lat).abs() < 1e-9 && (a.lon - b.lon).abs() < 1e-9
    }

    #[test]
    fn every_position_sits_on_the_lattice() {
        // Multiples of the spacing, which is what makes them stable. A
        // position computed from the viewport's own corner would pass the
        // pan test by luck and fail this.
        for zoom in [8.0, 10.0, 11.0, 13.0, 15.0] {
            let view = view_at(32.70, -117.25, zoom);
            let positions = barb_lattice(&view, GFS_DEG);
            assert!(!positions.is_empty(), "nothing at zoom {zoom}");

            // Recover the spacing from the two closest longitudes.
            let mut lons: Vec<f64> = positions.iter().map(|p| p.lon).collect();
            lons.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
            lons.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
            assert!(lons.len() > 1, "only one column at zoom {zoom}");
            let spacing = lons[1] - lons[0];

            for p in &positions {
                let steps = p.lon / spacing;
                assert!(
                    (steps - steps.round()).abs() < 1e-6,
                    "zoom {zoom}: longitude {} is not a multiple of {spacing}",
                    p.lon
                );
                let steps = p.lat / spacing;
                assert!(
                    (steps - steps.round()).abs() < 1e-6,
                    "zoom {zoom}: latitude {} is not a multiple of {spacing}",
                    p.lat
                );
            }
        }
    }

    #[test]
    fn the_spacing_tracks_the_zoom() {
        // Zoomed out, barbs should be further apart on the ground so they
        // stay a readable distance apart on screen. Without this the
        // lattice is either a thicket or empty.
        let spacing_at = |zoom: f64| {
            let p = barb_lattice(&view_at(32.70, -117.25, zoom), GFS_DEG);
            let mut lons: Vec<f64> = p.iter().map(|q| q.lon).collect();
            lons.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
            lons.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
            lons[1] - lons[0]
        };

        let coarse = spacing_at(8.0);
        let mid = spacing_at(11.0);
        let fine = spacing_at(14.0);
        assert!(
            coarse > mid && mid > fine,
            "spacing should shrink with zoom: {coarse}, {mid}, {fine}"
        );

        // And every level is a power-of-two multiple of the model's own
        // spacing, so the coarse levels land on real nodes.
        for spacing in [coarse, mid, fine] {
            let ratio = (spacing / GFS_DEG).log2();
            assert!(
                (ratio - ratio.round()).abs() < 1e-9,
                "{spacing} is not GFS's 0.25 times a power of two"
            );
        }
    }

    #[test]
    fn there_are_barbs_at_every_zoom_and_never_too_many() {
        // A readable screen holds around a hundred. The first version
        // clamped the subdivision three levels below the model's spacing,
        // and the layer silently emptied above zoom 14 — harbour zoom,
        // where someone is actually navigating.
        for zoom in [2.0, 5.0, 8.0, 11.0, 14.0, 16.0, 18.0, 20.0] {
            let n = barb_lattice(&view_at(32.70, -117.25, zoom), GFS_DEG).len();
            println!("zoom {zoom}: {n} barbs");
            assert!(n > 4, "zoom {zoom} produced only {n}");
            assert!(n <= 800, "zoom {zoom} produced {n}");
        }
    }

    #[test]
    fn a_forecast_with_no_spacing_produces_nothing() {
        // A malformed file could carry a zero or negative step, and the
        // loop below divides by it.
        assert!(barb_lattice(&view_at(32.7, -117.25, 11.0), 0.0).is_empty());
        assert!(barb_lattice(&view_at(32.7, -117.25, 11.0), -0.25).is_empty());
        assert!(barb_lattice(&view_at(32.7, -117.25, 11.0), f64::NAN).is_empty());
    }

    #[test]
    fn utc_times_are_formatted_from_the_epoch_correctly() {
        // 2026-03-01T15:25:00Z — the high water NOAA publishes for San
        // Diego that afternoon, so the arithmetic is checked against a real
        // instant rather than a round number.
        assert_eq!(format_utc_hhmm(1_772_378_700), "15:25Z");
        // Midnight and the second before it, the two the modulo gets wrong.
        assert_eq!(format_utc_hhmm(1_772_323_200), "00:00Z");
        assert_eq!(format_utc_hhmm(1_772_323_199), "23:59Z");
    }

    #[test]
    fn utc_formatting_survives_times_before_1970() {
        // `%` on a negative i64 is negative in Rust, which would print
        // "-1:-1Z". `rem_euclid` is what makes this right, and a clock that
        // has not been set yet is exactly when it would show.
        let formatted = format_utc_hhmm(-1);
        assert_eq!(formatted, "23:59Z");
        assert!(!formatted.contains('-'));
    }

    #[test]
    fn relative_times_read_the_way_they_are_spoken() {
        assert_eq!(format_away(0), "in 0m");
        assert_eq!(format_away(60), "in 1m");
        assert_eq!(format_away(11 * 60), "in 11m");
        assert_eq!(format_away(2 * 3600 + 11 * 60), "in 2h 11m");
        // Zero-padded minutes after an hour, so "2h 05m" not "2h 5m".
        assert_eq!(format_away(2 * 3600 + 5 * 60), "in 2h 05m");
        // Seconds are rounded down, not up: a high water is "in 0m" for its
        // last minute rather than jumping to "1m ago".
        assert_eq!(format_away(59), "in 0m");
    }

    #[test]
    fn past_times_are_marked_as_past() {
        assert_eq!(format_away(-12 * 60), "12m ago");
        assert_eq!(format_away(-(3 * 3600 + 30 * 60)), "3h 30m ago");
        // And not as a negative "in".
        assert!(!format_away(-60).starts_with("in"));
    }

    #[test]
    fn the_clock_is_plausible() {
        // Catches a sign error or a unit mix-up in `unix_now`, which would
        // otherwise show up as a tide prediction for the wrong century.
        let now = unix_now();
        // 2025-01-01 .. 2100-01-01.
        assert!(
            (1_735_689_600..4_102_444_800).contains(&now),
            "the clock says {now}"
        );
    }

    // ── The boat ────────────────────────────────────────────────────────

    use rutter_core::nmea::Stamped;

    /// A vessel with everything a full instrument feed would give.
    fn underway() -> Vessel {
        Vessel {
            position: Some(Stamped::new(Position::new(32.68, -117.23), 1_000)),
            sog_kn: Some(Stamped::new(5.2, 1_000)),
            cog_deg: Some(Stamped::new(184.4, 1_000)),
            heading_deg: Some(Stamped::new(190.0, 1_000)),
            depth_m: Some(Stamped::new(9.4, 1_000)),
            apparent_wind: Some(Stamped::new((45.0, 12.0), 1_000)),
            satellites: Some(11),
            fix_quality: 2,
        }
    }

    #[test]
    fn a_moored_boat_points_where_its_compass_says_and_not_where_gps_guesses() {
        // The failure: a GPS computes course from successive positions, so
        // a boat at rest produces a course from pure noise and the symbol
        // spins. With a compass aboard, the compass wins.
        let mut vessel = underway();
        vessel.sog_kn = Some(Stamped::new(0.1, 1_000));
        vessel.cog_deg = Some(Stamped::new(37.0, 1_000));

        assert_eq!(
            course_to_draw(&vessel),
            Some(190.0),
            "a stopped boat should use its heading"
        );

        // Making way, the course over ground is the one that matters: it is
        // where the boat is actually going, leeway and tide included.
        assert_eq!(course_to_draw(&underway()), Some(184.4));
    }

    #[test]
    fn a_boat_with_neither_course_nor_heading_points_nowhere() {
        // Rather than north. A symbol pointing north by default is a claim
        // about the heading, and a wrong one.
        let vessel = Vessel {
            position: Some(Stamped::new(Position::new(32.68, -117.23), 1_000)),
            ..Vessel::default()
        };
        assert_eq!(course_to_draw(&vessel), None);
    }

    #[test]
    fn a_stopped_boat_with_no_compass_still_points_nowhere() {
        let mut vessel = underway();
        vessel.heading_deg = None;
        vessel.sog_kn = Some(Stamped::new(0.0, 1_000));
        assert_eq!(course_to_draw(&vessel), None);
    }

    #[test]
    fn the_heading_line_appears_only_when_it_says_something_new() {
        let mut vessel = underway();

        // Six degrees of leeway: worth saying.
        assert_eq!(heading_line(&vessel), Some("HDG 190°".to_owned()));

        // Steering the course exactly: the second line would be noise.
        vessel.heading_deg = Some(Stamped::new(184.4, 1_000));
        assert_eq!(heading_line(&vessel), None);

        // Across north, where the naive subtraction gives 358 and would
        // have claimed a boat steering its course was 358 degrees off it.
        vessel.cog_deg = Some(Stamped::new(359.0, 1_000));
        vessel.heading_deg = Some(Stamped::new(1.0, 1_000));
        assert_eq!(
            heading_line(&vessel),
            None,
            "359 and 001 are two degrees apart, not 358"
        );

        // And a real difference across north is still reported.
        vessel.heading_deg = Some(Stamped::new(20.0, 1_000));
        assert_eq!(heading_line(&vessel), Some("HDG 020°".to_owned()));
    }

    #[test]
    fn motion_survives_a_half_equipped_receiver() {
        let mut vessel = underway();
        assert_eq!(motion_line(&vessel), Some("5.2 kn   COG 184°".to_owned()));

        vessel.cog_deg = None;
        assert_eq!(motion_line(&vessel), Some("5.2 kn".to_owned()));

        vessel.sog_kn = None;
        assert_eq!(motion_line(&vessel), None);

        vessel.cog_deg = Some(Stamped::new(7.0, 1_000));
        assert_eq!(
            motion_line(&vessel),
            Some("COG 007°".to_owned()),
            "a bearing is always three digits"
        );
    }

    #[test]
    fn apparent_wind_is_named_the_way_it_is_read_on_deck() {
        assert_eq!(
            apparent_wind_line((45.0, 12.0)),
            "12.0 kn apparent, 45° starboard"
        );
        // 315 relative is 45 on the port bow, not 315 of anything.
        assert_eq!(
            apparent_wind_line((315.0, 12.0)),
            "12.0 kn apparent, 45° port"
        );
        // Dead ahead and dead astern have no side to come over.
        assert_eq!(apparent_wind_line((0.0, 8.0)), "8.0 kn apparent, 0°");
        assert_eq!(apparent_wind_line((180.0, 8.0)), "8.0 kn apparent, 180°");
    }

    #[test]
    fn the_fix_age_reads_in_the_units_it_is_worth_hearing() {
        assert_eq!(fix_age_line(0), "fix is current");
        assert_eq!(fix_age_line(1), "fix is current");
        assert_eq!(fix_age_line(9), "fix 9 s old");
        assert_eq!(fix_age_line(59), "fix 59 s old");
        assert_eq!(fix_age_line(60), "fix 1 min old");
        assert_eq!(fix_age_line(3_600), "fix 60 min old");
    }

    #[test]
    fn a_searching_receiver_claims_no_fix_quality() {
        // Quality 0 is a GGA from a receiver that is powered and has not
        // found anything. Reporting it as a kind of fix would be a lie.
        let vessel = Vessel {
            fix_quality: 0,
            satellites: Some(3),
            ..underway()
        };
        assert_eq!(fix_quality_line(&vessel), None);

        assert_eq!(
            fix_quality_line(&underway()),
            Some("differential, 11 satellites".to_owned())
        );
    }

    #[test]
    fn a_position_reads_as_degrees_and_decimal_minutes() {
        // What a chart's margin is ruled in and what a position is given
        // over the radio in. The entrance to San Diego Bay.
        assert_eq!(
            format_position(Position::new(32.68, -117.23)),
            "32°40.80'N  117°13.80'W"
        );
        // Southern and eastern hemispheres, and a position where the
        // minutes need their leading zero.
        assert_eq!(
            format_position(Position::new(-33.8600, 151.2100)),
            "33°51.60'S  151°12.60'E"
        );
        assert_eq!(
            format_position(Position::new(32.005, -117.005)),
            "32°00.30'N  117°00.30'W"
        );
    }

    #[test]
    fn the_status_line_still_carries_the_position_it_used_to() {
        // The formatter was shared with the boat's readout; the view's own
        // line must not have changed shape in the process.
        let line = status_line(&view_at(32.68, -117.23, 12.0));
        assert!(
            line.starts_with("32°40.80'N  117°13.80'W"),
            "status line reads {line:?}"
        );
        assert!(line.contains("z12.0"), "status line reads {line:?}");
    }

    #[test]
    fn panning_stops_the_chart_following_the_boat() {
        // The worst thing a plotter can do is fight the hand on it: a view
        // that springs back to the boat the moment you let go makes it
        // impossible to look ahead.
        let mut app = App::new(Sources::default(), None);
        app.follow = true;
        app.pan_pixels(50.0, 0.0);
        assert!(!app.follow, "a drag should cancel follow");
    }

    #[test]
    fn there_is_nowhere_to_go_without_a_fix() {
        let mut app = App::new(Sources::default(), None);
        let before = app.view.centre;
        assert!(!app.go_to_boat_now(), "claimed to have a boat to go to");
        assert!(same_place(&app.view.centre, &before));
    }
}
