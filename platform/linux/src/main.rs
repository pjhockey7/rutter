//! Linux board support.
//!
//! Window, event loop, input, and finding the data files. Nothing about
//! charts, weather or tides is here — that is [`rutter_app`], which Android
//! drives too. This file used to hold both, and the result was that three
//! features existed on Linux and not on Android; the split exists so that
//! cannot happen again.
//!
//! Linux is the development target rather than the primary one. It is where
//! the renderer gets looked at, because the turnaround is a second instead of
//! an adb install, and because a bug that reproduces here is a bug in the
//! portable code rather than in the Android shim.

use std::sync::Arc;

use rutter_app::{App, NmeaEndpoint, Position, Sources, Viewport};
use rutter_charts::{ChartSource, ImagerySource};
use rutter_render::{Frame, GuiFrame, Renderer};
use rutter_tides::TideStations;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// Where `rutter noaa` and `rutter charts` write by default.
const DEFAULT_CHARTS: &str = "build/charts/noaa.mbtiles";
/// Where `rutter imagery` writes by default.
const DEFAULT_IMAGERY: &str = "build/charts/imagery.mbtiles";
/// Where `rutter tides` writes by default.
const DEFAULT_TIDES: &str = "build/charts/tides.sqlite";
/// Where `rutter weather` writes by default.
const DEFAULT_WEATHER: &str = "build/charts/weather.bin";

/// Pixels of pan per arrow-key press.
const KEY_PAN: f64 = 80.0;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let (sources, draft_m, opening) = match load_sources() {
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

    let mut platform = Platform {
        app: App::new(sources, draft_m),
        opening,
        ..Platform::default()
    };
    if let Err(e) = event_loop.run_app(&mut platform) {
        eprintln!("rutter: {e}");
        std::process::exit(1);
    }
}

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
/// Hand-parsed rather than with clap: there are four options, and a
/// dependency that has to cross-compile to four Android ABIs should buy more
/// than this.
/// Where to open, when `--at` or `--zoom` says so.
#[derive(Debug, Clone, Copy, Default)]
struct Opening {
    at: Option<Position>,
    zoom: Option<f64>,
}

fn load_sources() -> Result<(Sources, Option<f64>, Opening), String> {
    let mut args = std::env::args().skip(1);
    let mut chart_path = None;
    let mut imagery_path = None;
    let mut tide_path = None;
    let mut weather_path = None;
    let mut draft_m = None;
    let mut nmea: Option<NmeaEndpoint> = None;
    let mut at = None;
    let mut zoom = None;

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
            "--tides" | "-t" => {
                tide_path = Some(
                    args.next()
                        .ok_or("--tides needs a path to a .sqlite file")?,
                );
            }
            // Switches the under-keel check on at startup. Worth a flag
            // rather than only a panel control: a draft does not change
            // between runs, and typing it every time would be the kind of
            // friction that stops the feature being used.
            "--weather" | "-w" => {
                weather_path = Some(args.next().ok_or("--weather needs a path to a .bin file")?);
            }
            // A GPS or an instrument network, over the wire. No default:
            // an address cannot be guessed, and a plotter that quietly
            // probes the network on startup is not one anybody asked for.
            "--nmea" | "-n" => {
                let text = args
                    .next()
                    .ok_or("--nmea needs an address, like tcp://192.168.1.50:10110")?;
                nmea = Some(NmeaEndpoint::parse(&text).map_err(|e| format!("--nmea: {e}"))?);
            }
            "--draft" | "-d" => {
                let text = args.next().ok_or("--draft needs a depth in metres")?;
                let metres: f64 = text
                    .parse()
                    .map_err(|_| format!("--draft wants metres, got '{text}'"))?;
                if !(0.0..=20.0).contains(&metres) {
                    return Err(format!("--draft {metres} m is not a boat"));
                }
                draft_m = Some(metres);
            }
            // Where to open. Worth a flag for development: reproducing a
            // performance or drawing problem means getting the view over
            // the same water every time, and `fit_chart` lands wherever
            // the chart's own extent happens to be.
            "--at" => {
                let text = args.next().ok_or("--at needs <lat>,<lon>")?;
                let (lat, lon) = text
                    .split_once(',')
                    .ok_or_else(|| format!("expected --at <lat>,<lon>, got '{text}'"))?;
                let lat: f64 = lat
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad latitude '{lat}'"))?;
                let lon: f64 = lon
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad longitude '{lon}'"))?;
                at = Some(Position::new(lat, lon));
            }
            "--zoom" | "-z" => {
                let text = args.next().ok_or("--zoom needs a number")?;
                zoom = Some(text.parse().map_err(|_| format!("bad zoom '{text}'"))?);
            }
            "--help" | "-h" => {
                println!("usage: rutter [--charts FILE] [--imagery FILE] [--tides FILE]");
                println!("              [--draft METRES]");
                println!();
                println!("  --charts   vector chart, from `rutter charts` or `rutter noaa`");
                println!("  --imagery  aerial photography, from `rutter imagery`");
                println!("  --tides    tide stations, from `rutter tides`");
                println!("  --weather  a wind forecast, from `rutter weather`");
                println!("  --draft    your draft in metres; switches on the under-keel check");
                println!("  --nmea     a GPS or instrument feed; see below");
                println!("  --at       open here, as <lat>,<lon>, instead of the chart centre");
                println!("  --zoom     open at this zoom");
                println!();
                println!("Each defaults to what its command writes:");
                println!("  {DEFAULT_CHARTS}");
                println!("  {DEFAULT_IMAGERY}");
                println!("  {DEFAULT_TIDES}");
                println!("  {DEFAULT_WEATHER}");
                println!();
                println!("--nmea has no default. Common sources:");
                println!("  /dev/ttyUSB0           a USB GPS dongle, 4800 baud");
                println!("  /dev/ttyACM0@38400     a dongle at a stated rate");
                println!("  tcp://<phone>:10110    a GPS relay app on a phone");
                println!("  tcp://<plotter>:10110  a chartplotter or multiplexer");
                println!("  udp://:10110           anything broadcasting to the boat network");
                println!();
                println!("  drag or arrows  pan");
                println!("  scroll or +/-   zoom");
                println!("  0               fit the chart");
                println!("  b               centre on the boat");
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

    let tides = match tide_path.or_else(|| default_path(DEFAULT_TIDES)) {
        Some(path) => {
            let source = TideStations::open(&path).map_err(|e| format!("{path}: {e}"))?;
            let count = source.count().map_err(|e| format!("{path}: {e}"))?;
            println!("tides: {count} stations");
            if let Some(credit) = source.attribution() {
                println!("  {credit}");
            }
            Some(source)
        }
        None => {
            println!("tides: none — fetch some with `rutter tides --near <lat>,<lon>`");
            None
        }
    };

    if let Some(endpoint) = &nmea {
        println!("nmea: {} {}", endpoint.protocol_name(), endpoint.address());
    } else {
        println!("nmea: none — pass --nmea tcp://<host>:10110 for a GPS");
    }

    let weather = match weather_path.or_else(|| default_path(DEFAULT_WEATHER)) {
        Some(path) => {
            let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            let forecast =
                rutter_app::Forecast::parse(&bytes).map_err(|e| format!("{path}: {e}"))?;
            let hours: Vec<i32> = forecast.steps.iter().map(|s| s.hour).collect();
            println!("weather: GFS run {}, hours {hours:?}", forecast.run);
            Some(forecast)
        }
        None => {
            println!("weather: none — fetch some with `rutter weather --chart <file>`");
            None
        }
    };

    Ok((
        Sources {
            charts,
            imagery,
            tides,
            weather,
            nmea,
            // Linux has no system location service worth the name:
            // GeoClue is a D-Bus daemon that mostly asks Mozilla where
            // your WiFi is, which is not navigation. A GPS on a laptop is
            // a dongle on a serial port, and `--nmea /dev/ttyUSB0` is
            // already that.
            gps: None,
        },
        draft_m,
        Opening { at, zoom },
    ))
}

/// The window, the GPU, egui's winit glue — and the app they drive.
#[derive(Default)]
struct Platform {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    egui: Option<EguiState>,
    app: App,
    /// Cursor position, kept so a drag can be measured and so a scroll can
    /// zoom about the pointer rather than the centre.
    cursor: Option<(f64, f64)>,
    dragging: bool,
    /// What `--at` and `--zoom` asked for, applied once on first resume.
    opening: Opening,
}

/// egui's context plus its winit translator.
///
/// Kept here rather than in `rutter-app`: the context is platform-independent
/// but the winit state is not, and Android translates its own events.
struct EguiState {
    ctx: egui::Context,
    winit: egui_winit::State,
}

impl Platform {
    /// Ask for another frame, if there is a window to ask.
    fn redraw_soon(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };

        let started = std::time::Instant::now();
        if let Some(renderer) = self.renderer.as_mut() {
            self.app.refresh(renderer);
        }
        let refreshed = started.elapsed();

        // Built before the egui closure, because it reads the app while the
        // closure writes it.
        let pixels_per_point = self.egui.as_ref().map_or(1.0, |e| e.ctx.pixels_per_point());
        let state = self
            .renderer
            .as_ref()
            .map(|r| self.app.panel_state(r, pixels_per_point));

        // egui is immediate-mode: the panel is rebuilt from scratch every
        // frame and reports what was clicked. Actions are applied *after*
        // the frame is drawn, so a toggle takes effect on the next one —
        // which is why a redraw is requested below.
        let mut actions = Vec::new();
        let gui_output = match (self.egui.as_mut(), state.as_ref()) {
            (Some(egui), Some(state)) => {
                let input = egui.winit.take_egui_input(&window);
                let app = &mut self.app;
                // `run_ui`, not `run`: it hands the closure the root Ui,
                // which is what the panels attach to. It takes an FnMut and
                // may call it more than once for a single frame, so this
                // assigns rather than accumulates — appending would double
                // every click.
                let output = egui.ctx.run_ui(input, |ui| {
                    actions = app.show(ui, state);
                });
                egui.winit
                    .handle_platform_output(&window, output.platform_output.clone());
                Some(output)
            }
            _ => None,
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

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let frame = renderer.render_with_ui(&self.app.view, gui);
        let changed = self.app.apply(&actions, renderer);

        // The refresh figure is the one that matters: it is the work the
        // frame does itself, as opposed to waiting for vsync. A chart
        // decode happening on the worker should not appear in it at all.
        let refresh_ms = refreshed.as_secs_f64() * 1000.0;
        if refresh_ms > 4.0 {
            log::debug!("slow frame: {refresh_ms:.1} ms of refresh");
        }
        log::trace!(
            "frame: {:.1} ms total, {refresh_ms:.1} ms refresh",
            started.elapsed().as_secs_f64() * 1000.0
        );

        if changed || wants_repaint {
            // egui reports when it wants drawing again — mid-animation, or
            // while a widget is hovered — as a per-viewport delay. Zero
            // means "immediately"; anything else means the panel has settled
            // and the event loop can sleep, which is the point of
            // ControlFlow::Wait on a battery.
            window.request_redraw();
        }

        match frame {
            Frame::Drawn | Frame::Skipped => {}
            // The renderer already fixed the configuration; it just needs
            // asking again.
            Frame::Reconfigured => self.redraw_soon(),
            // Rebuilt from the window on the next resume. On Linux this
            // should not happen, so say so rather than silently recovering.
            Frame::SurfaceLost => {
                log::warn!("surface lost; rebuilding");
                self.renderer = None;
                self.window = None;
                self.app.forget_uploads();
            }
        }
    }
}

impl ApplicationHandler for Platform {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // resumed fires once on Linux and on every foreground on Android,
        // which is why the renderer is built here rather than in main.
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
        self.app.resize(size.width, size.height);
        // Open on the chart rather than on the hard-coded default, so that
        // passing --charts shows the data instead of empty sea next to it.
        // `--at` and `--zoom` override it.
        self.app.fit_chart();
        if let Some(at) = self.opening.at {
            self.app.view.centre = at;
        }
        if let Some(zoom) = self.opening.zoom {
            self.app.view.zoom = zoom.clamp(Viewport::MIN_ZOOM, Viewport::MAX_ZOOM);
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
        log::info!(
            "surface {}x{} at {} px/pt",
            size.width,
            size.height,
            window.scale_factor()
        );
        self.egui = Some(EguiState { ctx, winit });

        self.window = Some(window);
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
                self.app.resize(size.width, size.height);
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
                    self.app.pan_pixels(now.0 - last.0, now.1 - last.1);
                    self.redraw_soon();
                }
                self.cursor = Some(now);
            }

            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.dragging = false;
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    // A touchpad reports pixels; 50 per zoom step is about
                    // one comfortable two-finger swipe.
                    MouseScrollDelta::PixelDelta(p) => p.y / 50.0,
                };
                // About the pointer, not the centre: the feature under the
                // cursor is the one being looked at.
                let (x, y) = self.cursor.unwrap_or((
                    f64::from(self.app.view.width_px) / 2.0,
                    f64::from(self.app.view.height_px) / 2.0,
                ));
                self.app.zoom_about(steps, x, y);
                self.redraw_soon();
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mut changed = true;
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Named(NamedKey::ArrowUp) => self.app.pan_pixels(0.0, KEY_PAN),
                    Key::Named(NamedKey::ArrowDown) => self.app.pan_pixels(0.0, -KEY_PAN),
                    Key::Named(NamedKey::ArrowLeft) => self.app.pan_pixels(KEY_PAN, 0.0),
                    Key::Named(NamedKey::ArrowRight) => self.app.pan_pixels(-KEY_PAN, 0.0),
                    Key::Character("+" | "=") => self.app.zoom_by(4.0),
                    Key::Character("-" | "_") => self.app.zoom_by(-4.0),
                    // Back to the chart. This used to jump to a hard-coded
                    // position in Cornwall, left behind when the default
                    // view moved to San Diego — so "home" took you 5000
                    // miles from your charts.
                    Key::Character("0") => changed = self.app.fit_chart(),
                    // To the boat. The one place worth a key of its own:
                    // on the water it is where you look back to after
                    // every detour across the chart.
                    Key::Character("b" | "B") => changed = self.app.go_to_boat_now(),
                    _ => changed = false,
                }
                if changed {
                    self.redraw_soon();
                }
            }

            _ => {}
        }
    }
}
