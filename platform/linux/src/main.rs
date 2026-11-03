//! Linux board support.
//!
//! Window, event loop, input. Nothing about charts, weather or tides appears
//! here — if it did, it would have to be written again for Android, and that
//! is exactly the duplication this layout exists to prevent.
//!
//! Linux is the development target rather than the primary one. It is where
//! the renderer gets looked at, because the turnaround is a second instead of
//! an adb install, and because a bug that reproduces here is a bug in the
//! portable code rather than in the Android shim.

use std::sync::Arc;

use rutter_charts::{ChartSource, ImagerySource};
use rutter_core::chart::TileId;
use rutter_core::{chart, LatLon, View};
use rutter_render::{Action, Frame, GuiFrame, ImageryState, LayerEntry, Renderer, Ui};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let (charts, imagery) = match load_sources() {
        Ok(pair) => pair,
        Err(message) => {
            eprintln!("rutter: {message}");
            std::process::exit(1);
        }
    };

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("rutter: no display — {e}");
            std::process::exit(1);
        }
    };

    // Wait rather than Poll: a chart that is not moving should not be
    // redrawn, and the difference on a laptop is several watts.
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        charts,
        imagery,
        // On by default when supplied: nobody passes --imagery and then
        // wants it hidden.
        imagery_visible: true,
        ..App::default()
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("rutter: {e}");
        std::process::exit(1);
    }
}

/// The text in the top strip: where the view is and how far in.
///
/// Degrees and decimal minutes, which is how a position is read aloud and
/// written in a log — not decimal degrees, which no one on a boat uses.
fn status_line(view: &View) -> String {
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

/// Where `rutter noaa` and `rutter charts` write by default.
const DEFAULT_CHARTS: &str = "build/charts/noaa.mbtiles";
/// Where `rutter imagery` writes by default.
const DEFAULT_IMAGERY: &str = "build/charts/imagery.mbtiles";

/// The path if something is actually there, otherwise nothing.
///
/// Without this fallback, fetching data and then running the app shows an
/// empty sea and no explanation — you have to know to pass a flag naming a
/// file the tool just created at a path it chose itself. An explicit flag
/// still wins, so this only decides what happens when none is given.
fn default_path(path: &str) -> Option<String> {
    std::path::Path::new(path).exists().then(|| path.to_owned())
}

/// Read the command line and open whatever sources were named, falling back
/// to the default paths above.
///
/// Hand-parsed rather than with clap: there are two options, and a
/// dependency that has to cross-compile to four Android ABIs should buy more
/// than this.
fn load_sources() -> Result<(Option<ChartSource>, Option<ImagerySource>), String> {
    let mut args = std::env::args().skip(1);
    let mut chart_path = None;
    let mut imagery_path = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--charts" | "-c" => {
                chart_path = Some(
                    args.next()
                        .ok_or("--charts needs a path to an .mbtiles file")?,
                );
            }
            "--imagery" | "-i" => {
                imagery_path = Some(
                    args.next()
                        .ok_or("--imagery needs a path to an .mbtiles file")?,
                );
            }
            "--help" | "-h" => {
                println!("usage: rutter [--charts <file.mbtiles>] [--imagery <file.mbtiles>]");
                println!();
                println!("  --charts   vector chart, from `rutter charts` or `rutter noaa`");
                println!("  --imagery  aerial photography, from `rutter imagery`");
                println!();
                println!("Both default to what those commands write:");
                println!("  {DEFAULT_CHARTS}");
                println!("  {DEFAULT_IMAGERY}");
                println!();
                println!("  drag or arrows  pan");
                println!("  scroll or +/-   zoom");
                println!("  0               home");
                println!("  Escape          quit");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option '{other}' — try --help")),
        }
    }

    let charts = match chart_path.or_else(|| default_path(DEFAULT_CHARTS)) {
        Some(path) => {
            // No layer filter at open: the panel starts with everything
            // visible and narrows from there.
            let source = ChartSource::open(&path, &[]).map_err(|e| format!("{path}: {e}"))?;
            let meta = source.metadata();
            if !meta.is_vector() {
                return Err(format!(
                    "{path}: format '{}' is raster — pass it as --imagery",
                    meta.format
                ));
            }
            println!(
                "charts: {} — zoom {}..{}, {} layers",
                if meta.name.is_empty() {
                    path.as_str()
                } else {
                    meta.name.as_str()
                },
                meta.min_zoom,
                meta.max_zoom,
                meta.layers.len()
            );
            Some(source)
        }
        None => {
            println!("charts: none — fetch some with `rutter noaa --near <lat>,<lon>`");
            None
        }
    };

    let imagery = match imagery_path.or_else(|| default_path(DEFAULT_IMAGERY)) {
        Some(path) => {
            let source = ImagerySource::open(&path).map_err(|e| format!("{path}: {e}"))?;
            let meta = source.metadata();
            println!(
                "imagery: {} — zoom {}..{}",
                if meta.name.is_empty() {
                    path.as_str()
                } else {
                    meta.name.as_str()
                },
                meta.min_zoom,
                meta.max_zoom
            );
            if let Some(credit) = &meta.attribution {
                println!("  {credit}");
            }
            Some(source)
        }
        None => {
            println!("imagery: none — build some with `rutter imagery --chart <file>`");
            None
        }
    };

    Ok((charts, imagery))
}

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    view: View,
    /// Cursor position, kept so a drag can be measured.
    cursor: Option<(f64, f64)>,
    dragging: bool,
    charts: Option<ChartSource>,
    /// The tile set the renderer's chart buffer was built from.
    ///
    /// Kept so that panning within one tile — most of a drag — does not
    /// re-decode and re-upload geometry that has not changed. This is the
    /// whole reason the renderer stores chart vertices against their own
    /// origin rather than the view's.
    loaded: Vec<TileId>,

    imagery: Option<ImagerySource>,
    /// Whether the imagery underlay is switched on.
    imagery_visible: bool,
    /// The tile set the renderer's imagery textures were built from, so a
    /// pan within the same tiles does not re-decode a hundred JPEGs.
    imagery_loaded: Vec<TileId>,

    /// Panel state, and egui's own.
    ui: Ui,
    egui: Option<EguiState>,
    /// What the layer panel shows. Rebuilt when the chart or the selection
    /// changes, not every frame.
    layer_entries: Vec<LayerEntry>,
}

/// egui's context plus its winit translator.
///
/// Kept beside the renderer rather than inside it: the context is
/// platform-independent but the winit state is not, and `rutter-render` must
/// stay free of winit so Android can drive it differently.
struct EguiState {
    ctx: egui::Context,
    winit: egui_winit::State,
}

impl App {
    /// Pan by a screen-space delta in pixels.
    ///
    /// Done through the projection rather than by adding degrees to the
    /// centre: a degree of longitude is not a fixed number of pixels, so the
    /// naive version drags at the wrong speed everywhere except the equator
    /// and drags the wrong way near the poles.
    fn pan_pixels(&mut self, dx: f64, dy: f64) {
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
    fn refresh_charts(&mut self) {
        let Some(source) = self.charts.as_mut() else {
            return;
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        let got = source.collect(&self.view);

        if got.tiles == self.loaded {
            return;
        }

        if let Some(wanted) = got.over_budget {
            // Zoomed far outside the chart's range. Clear rather than leave
            // the previous zoom's geometry on screen pretending to be this
            // one.
            log::debug!("charts: {wanted} tiles wanted, over budget — not drawing");
            renderer.clear_chart();
            self.loaded = got.tiles;
            return;
        }

        // One unreadable tile should not take the chart down: a corrupt blob
        // in the middle of a file still leaves the rest navigable.
        for (id, why) in &got.failed {
            log::warn!("{id:?}: {why}");
        }

        renderer.set_chart(&got.lines, &got.points);

        log::debug!(
            "charts: {} tiles, {} polylines, {} points, {} vertices, styles {:?}",
            got.tiles.len(),
            got.lines.len(),
            got.points.len(),
            renderer.chart_vertex_count(),
            got.styles()
        );

        self.loaded = got.tiles;
    }

    /// Rebuild the panel's layer list from the chart and the current
    /// selection.
    ///
    /// Only called when something changed, not per frame: a real ENC chart
    /// has sixty-odd layers and allocating that list sixty times a second to
    /// draw the same panel would be waste.
    fn rebuild_layer_entries(&mut self) {
        self.layer_entries.clear();
        let Some(source) = &self.charts else {
            return;
        };

        let selected = source.selected_layers();
        // An empty selection means "all visible", which is the state a
        // freshly opened chart is in.
        let all_visible = selected.is_empty();

        for layer in &source.metadata().layers {
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
    fn apply(&mut self, actions: &[Action]) -> bool {
        if actions.is_empty() {
            return false;
        }
        // Imagery lives on the renderer, not the chart source, so those
        // actions are handled before the source is borrowed below.
        let mut touched = false;
        for action in actions {
            match action {
                Action::SetImageryOpacity(opacity) => {
                    if let Some(renderer) = self.renderer.as_mut() {
                        renderer.set_imagery_opacity(*opacity);
                        touched = true;
                    }
                }
                Action::SetImagery(on) => {
                    self.imagery_visible = *on;
                    // The tile set is unchanged — only whether it is drawn
                    // — so refresh_imagery needs nudging.
                    self.imagery_loaded.clear();
                    touched = true;
                }
                _ => {}
            }
        }

        let Some(source) = self.charts.as_mut() else {
            // Imagery and fit still work without a vector chart.
            return touched;
        };

        let mut changed = touched;
        let mut fit = false;

        for action in actions {
            match action {
                Action::SetLayer { name, visible } => {
                    changed |= source.toggle_layer(name, *visible);
                }
                Action::SetStyle { style, visible } => {
                    // Every layer in the group, one at a time. Clearer than
                    // a bulk set, and toggle_layer already handles the
                    // "empty means all" expansion.
                    let names: Vec<String> = source
                        .metadata()
                        .layers
                        .iter()
                        .filter(|l| rutter_charts::style_for_layer(&l.name) == *style)
                        .map(|l| l.name.clone())
                        .collect();
                    for name in names {
                        changed |= source.toggle_layer(&name, *visible);
                    }
                }
                Action::ShowAll => {
                    changed |= source.set_layers(&[]);
                }
                Action::HideAll => {
                    // One name that cannot match anything. An empty list
                    // means "all", so it cannot express "none".
                    changed |= source.set_layers(&["\0none".to_owned()]);
                }
                Action::FitChart => fit = true,
                // Both handled above, before the source was borrowed.
                Action::SetImagery(_) | Action::SetImageryOpacity(_) => {}
            }
        }

        if fit {
            if let Some(b) = source.metadata().bounds {
                self.view.centre = LatLon::new((b[1] + b[3]) / 2.0, (b[0] + b[2]) / 2.0);
                self.view.zoom = f64::from(source.metadata().max_zoom);
                changed = true;
            }
        }

        if changed {
            // The tile set may be identical, so refresh_charts would
            // short-circuit; force it to reupload.
            self.loaded.clear();
            self.rebuild_layer_entries();
        }
        changed
    }

    /// Reload imagery textures if the visible tile set has changed.
    ///
    /// Separate from `refresh_charts` because the two have different costs:
    /// a vector tile is a protobuf parse, an imagery tile is a JPEG decode
    /// and a quarter-megabyte upload.
    fn refresh_imagery(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

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

    fn redraw(&mut self) {
        self.refresh_charts();
        self.refresh_imagery();

        let Some(window) = self.window.clone() else {
            return;
        };

        // Run the interface for this frame, then composite it.
        //
        // egui is immediate-mode: the panel is rebuilt from scratch every
        // frame and reports what was clicked. Actions are applied *after*
        // the frame is drawn, so a toggle takes effect on the next one —
        // which is why `request_redraw` follows.
        let mut actions = Vec::new();
        let gui_output = self.egui.as_mut().map(|egui| {
            let input = egui.winit.take_egui_input(&window);
            let imagery_state = ImageryState {
                available: self.imagery.is_some(),
                visible: self.imagery_visible,
                opacity: self
                    .renderer
                    .as_ref()
                    .map_or(0.75, rutter_render::Renderer::imagery_opacity),
                tiles: self
                    .renderer
                    .as_ref()
                    .map_or(0, rutter_render::Renderer::imagery_tile_count),
                attribution: self
                    .imagery
                    .as_ref()
                    .and_then(|i| i.metadata().attribution.clone()),
            };
            // `run_ui`, not `run`: it hands the closure the root Ui, which
            // is what the panels attach to. It takes an FnMut and may call
            // it more than once for a single frame, so this assigns rather
            // than accumulates — appending would double every click.
            let output = egui.ctx.run_ui(input, |ui| {
                actions = self.ui.show(
                    ui,
                    &self.layer_entries,
                    &imagery_state,
                    &status_line(&self.view),
                );
            });
            egui.winit
                .handle_platform_output(&window, output.platform_output.clone());
            output
        });

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        // Tessellate, then hand the renderer the primitives plus the texture
        // delta *by value* — the delta has to be consumed, not borrowed, or
        // its Drop assertion fires.
        let tessellated = gui_output.map(|output| {
            let primitives = self
                .egui
                .as_ref()
                .map(|e| e.ctx.tessellate(output.shapes, output.pixels_per_point))
                .unwrap_or_default();
            (
                output.pixels_per_point,
                primitives,
                output.textures_delta,
                output.viewport_output,
            )
        });

        let (gui, wants_repaint) = match tessellated {
            Some((ppp, ref primitives, textures, viewports)) => (
                Some(GuiFrame {
                    primitives,
                    textures,
                    pixels_per_point: ppp,
                }),
                viewports.values().any(|v| v.repaint_delay.is_zero()),
            ),
            None => (None, false),
        };

        let frame = renderer.render_with_ui(&self.view, gui);

        // Apply now that the borrow of the renderer is done.
        if self.apply(&actions) {
            window.request_redraw();
        }
        // egui reports when it wants drawing again — mid-animation, or while
        // a widget is hovered — as a per-viewport delay. Zero means
        // "immediately"; anything else means the panel has settled and the
        // event loop can sleep, which is the point of ControlFlow::Wait on a
        // battery.
        if wants_repaint {
            window.request_redraw();
        }

        match frame {
            Frame::Drawn | Frame::Skipped => {}
            // The renderer already fixed the configuration; it just needs
            // asking again.
            Frame::Reconfigured => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            // Rebuilt from the window on the next resume. On Linux this
            // should not happen, so say so rather than silently recovering.
            Frame::SurfaceLost => {
                log::warn!("surface lost; rebuilding");
                self.renderer = None;
                self.window = None;
                // The chart buffer and every imagery texture died with
                // the renderer, so the record of what was uploaded is a lie.
                self.loaded.clear();
                self.imagery_loaded.clear();
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // resumed fires once on Linux and on every foreground on Android,
        // which is why the renderer is rebuilt here rather than in main.
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("rutter")
            .with_inner_size(winit::dpi::LogicalSize::new(1280, 800));

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("rutter: could not open a window — {e}");
                std::process::exit(1);
            }
        };

        let size = window.inner_size();
        self.view.width_px = size.width.max(1);
        self.view.height_px = size.height.max(1);

        // Open on the chart rather than on the hard-coded default, so that
        // passing --charts shows the data instead of empty sea next to it.
        if let Some(source) = &self.charts {
            let meta = source.metadata();
            if let Some(b) = meta.bounds {
                self.view.centre = LatLon::new((b[1] + b[3]) / 2.0, (b[0] + b[2]) / 2.0);
                self.view.zoom = f64::from(meta.max_zoom);
            }
        }

        // pollster rather than tokio: this is the one await in the whole
        // program, and it happens once at startup.
        match pollster::block_on(Renderer::new(window.clone(), size.width, size.height)) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("rutter: {e}");
                std::process::exit(1);
            }
        }

        // egui's context and its winit translator. The viewport id is
        // egui's own notion of "the main window".
        let ctx = egui::Context::default();
        let winit = egui_winit::State::new(
            ctx.clone(),
            ctx.viewport_id(),
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        self.egui = Some(EguiState { ctx, winit });

        self.window = Some(window);
        self.rebuild_layer_entries();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Offer every event to the interface first. When egui consumes it —
        // a click on a checkbox, a drag on the panel's edge — the chart must
        // not also act on it, or clicking a layer name would pan the chart
        // underneath the panel.
        let consumed = match (self.egui.as_mut(), self.window.as_ref()) {
            (Some(egui), Some(window)) => {
                let response = egui.winit.on_window_event(window, &event);
                if response.repaint {
                    window.request_redraw();
                }
                response.consumed
            }
            _ => false,
        };

        // Resize and redraw still have to reach the renderer even when egui
        // wants them, and closing the window is never egui's decision.
        let always = matches!(
            event,
            WindowEvent::CloseRequested
                | WindowEvent::Resized(_)
                | WindowEvent::RedrawRequested
                | WindowEvent::ScaleFactorChanged { .. }
        );
        if consumed && !always {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                self.view.width_px = size.width.max(1);
                self.view.height_px = size.height.max(1);
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
            }

            WindowEvent::RedrawRequested => self.redraw(),

            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = state == ElementState::Pressed;
            }

            WindowEvent::CursorMoved { position, .. } => {
                let now = (position.x, position.y);
                if let (true, Some(last)) = (self.dragging, self.cursor) {
                    self.pan_pixels(now.0 - last.0, now.1 - last.1);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                self.cursor = Some(now);
            }

            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.dragging = false;
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    // A touchpad reports pixels; 50 per zoom level is about
                    // one comfortable two-finger swipe.
                    MouseScrollDelta::PixelDelta(p) => p.y / 50.0,
                };
                self.view = self.view.zoomed(steps * 0.25);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mut changed = true;
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Named(NamedKey::ArrowUp) => self.pan_pixels(0.0, 80.0),
                    Key::Named(NamedKey::ArrowDown) => self.pan_pixels(0.0, -80.0),
                    Key::Named(NamedKey::ArrowLeft) => self.pan_pixels(80.0, 0.0),
                    Key::Named(NamedKey::ArrowRight) => self.pan_pixels(-80.0, 0.0),
                    Key::Character("+" | "=") => self.view = self.view.zoomed(1.0),
                    Key::Character("-" | "_") => self.view = self.view.zoomed(-1.0),
                    Key::Character("0") => {
                        self.view = View {
                            centre: LatLon::new(50.1531, -5.0659),
                            ..self.view
                        };
                    }
                    _ => changed = false,
                }
                if changed {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            _ => {}
        }
    }
}
