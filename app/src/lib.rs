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
pub use charts::ChartWorker;

use rutter_charts::{ChartSource, Collected, ImagerySource};
use rutter_core::chart::TileId;
use rutter_core::depth::{Draft, TideWindow};
use rutter_core::tide::{Kind, Station};
use rutter_core::{chart, LatLon, View};
use rutter_render::{
    Action, DraftState, ImageryState, LayerEntry, Renderer, TideMarker, TideReading, TideState, Ui,
};
use rutter_tides::TideStations;

// Re-exported for the platform shims. They need a position and a viewport to
// translate input into, and reaching past this crate to `rutter-core` for
// them would make the layering a suggestion rather than a rule.
pub use rutter_core::{LatLon as Position, View as Viewport};

/// The data files a platform found, however it found them.
#[derive(Debug, Default)]
pub struct Sources {
    pub charts: Option<ChartSource>,
    pub imagery: Option<ImagerySource>,
    pub tides: Option<TideStations>,
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

/// Everything the panel needs for one frame.
///
/// Built from `&App` and owned, so that the egui closure can borrow the app
/// mutably while still reading it.
#[derive(Debug)]
pub struct PanelState {
    layers: Vec<LayerEntry>,
    imagery: ImageryState,
    tides: TideState,
    draft: DraftState,
    status: String,
}

/// `Debug` is derived rather than hand-written because every field has it:
/// the sources print as their metadata, not their contents, and `Ui` holds
/// only presentation state.
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

    /// Last computed tide readings, and the second they were computed for.
    ///
    /// The curve and the high/low list cost about 0.6 ms together, which is
    /// worth having once a second and not sixty times. Markers are *not*
    /// cached: they move with the view and are only a projection each.
    tide_cache: Option<TideCache>,

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
        // First: the chart refresh reads the tide window to colour the
        // under-keel check, so a stale cache would colour against last
        // second's tide.
        self.refresh_tide_cache();
        self.refresh_charts(renderer);
        self.refresh_imagery(renderer);
    }

    /// Gather what the panel needs. See [`PanelState`].
    pub fn panel_state(&self, renderer: &Renderer, pixels_per_point: f32) -> PanelState {
        PanelState {
            layers: self.layer_entries.clone(),
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
            status: status_line(&self.view),
        }
    }

    /// Draw the panel for one frame and report what the user did.
    pub fn show(&mut self, ui: &mut egui::Ui, state: &PanelState) -> Vec<Action> {
        self.ui.show(
            ui,
            &state.layers,
            &state.imagery,
            &state.tides,
            &state.draft,
            &state.status,
        )
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
                | Action::SetDraft(_) => {}
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

    /// Which station to predict for: the pinned one, or the closest to the
    /// middle of the screen.
    ///
    /// Nearest-to-centre rather than nearest-to-boat because there is no
    /// GPS yet. When there is, the boat's position is the better default and
    /// this is the one line that changes.
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
        self.stations.iter().min_by(|a, b| {
            let da = self.view.centre.distance_to(LatLon::new(a.lat, a.lon));
            let db = self.view.centre.distance_to(LatLon::new(b.lat, b.lon));
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
    let (lat, lon) = (view.centre.lat, view.centre.lon);

    let fmt = |value: f64, positive: char, negative: char| {
        let hemisphere = if value >= 0.0 { positive } else { negative };
        let magnitude = value.abs();
        let degrees = magnitude.trunc();
        let minutes = (magnitude - degrees) * 60.0;
        format!("{degrees:.0}°{minutes:05.2}'{hemisphere}")
    };

    let scale = chart::metres_per_pixel(lat, view.tile_zoom());
    format!(
        "{}  {}   z{:.1}   {:.0} m/px",
        fmt(lat, 'N', 'S'),
        fmt(lon, 'E', 'W'),
        view.zoom,
        scale
    )
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
}
