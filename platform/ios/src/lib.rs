//! iOS board support.
//!
//! The third platform, after Linux and Android, and the one that has
//! never been built. **Nothing in this crate has been compiled against
//! the iOS SDK or run on a device** — that needs a Mac, and this was
//! written on a Linux laptop.
//!
//! What *has* been done is type-checking. Every Apple-specific module
//! here checks cleanly for `aarch64-apple-ios`, which catches a wrong
//! type, a missing argument and a method that does not exist in the
//! bindings. It does not catch a wrong selector — `msg_send!` resolves
//! names at runtime — nor anything about linking, signing or the bundle.
//! Treat the first build on a Mac as a debugging session, not a
//! formality, and start with `src/location.rs`, which is the file with
//! the most ways to be quietly wrong.
//!
//! ## How it is put together
//!
//! Three things differ from a desktop, and all three are the same
//! problems Android had:
//!
//! * **Touch, not a mouse.** One finger has to drive both a slider and a
//!   chart pan, so raw touches are translated to egui pointer events by
//!   hand. The press is always offered to the panel and who owns the
//!   gesture is decided a frame later — see [`Platform::settle_gesture`],
//!   and the note there, which is the fix for a bug that made every
//!   control need two taps on Android.
//! * **A notch.** `src/insets.rs` asks UIKit for the safe area and the
//!   chart is letterboxed into it.
//! * **Density.** Controls and the boat are laid out in physical pixels
//!   and a phone has two or three per point.
//!
//! ## There is no `main`
//!
//! This builds as a static library. The app bundle's `main` is Apple's —
//! a one-line `main.m` that calls [`rutter_main`]. See `ios/README.md`
//! for the build, which is a `cargo build` and an `xcodebuild`.

// Objective-C is unsafe by nature, and this crate is the board support —
// which is where the workspace's rule says unsafe belongs.
#![allow(unsafe_code)]

use std::sync::Arc;

use rutter_app::{App, Sources};
use rutter_render::{Frame, GuiFrame, Renderer};
use winit::application::ApplicationHandler;
use winit::event::{Touch, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

#[cfg(target_vendor = "apple")]
mod insets;
#[cfg(target_vendor = "apple")]
mod keepalive;
#[cfg(target_vendor = "apple")]
mod location;
#[cfg(target_vendor = "apple")]
mod paths;
#[cfg(target_vendor = "apple")]
mod siren;
#[cfg(target_vendor = "apple")]
mod zone;

/// Off-Apple there is no UIKit to ask and `Platform` still has to
/// compile, so that `cargo check --workspace` on a Linux laptop covers
/// everything in this file that is not Objective-C.
#[cfg(not(target_vendor = "apple"))]
// Unused off-Apple by construction — `read_insets` returns `None` there
// without consulting it. It exists so the shape is the same on both
// sides and the rest of this file compiles.
#[allow(dead_code)]
mod insets {
    #[derive(Debug, Clone, Copy, Default, PartialEq)]
    pub struct Insets {
        pub left: f64,
        pub top: f64,
        pub right: f64,
        pub bottom: f64,
    }

    impl Insets {
        pub fn any(self) -> bool {
            false
        }
    }
}

/// The app's one entry point, called from the bundle's `main.m`.
///
/// Never returns: winit's iOS backend calls `UIApplicationMain`, which
/// owns the thread for the life of the app.
///
/// # Safety
///
/// Must be called once, from the main thread, before anything else
/// touches UIKit. Both are guaranteed by being called from `main`.
#[unsafe(no_mangle)]
pub extern "C" fn rutter_main() {
    // Panics would otherwise unwind into Objective-C, which is undefined
    // behaviour rather than a crash report. A hook at least gets the
    // message into the device log before it goes.
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
    }));

    log::info!("rutter starting");

    let sources = open_bundled_sources();

    let event_loop = match EventLoop::new() {
        Ok(it) => it,
        Err(e) => {
            log::error!("could not build the event loop: {e}");
            return;
        }
    };

    let mut platform = Platform {
        // No draft: the under-keel check is switched on from the panel,
        // the same as on a desktop with no `--draft`.
        app: App::new(sources, None),
        ..Platform::default()
    };
    if let Err(e) = event_loop.run_app(&mut platform) {
        log::error!("event loop ended: {e}");
    }
}

/// What shipped in the bundle, plus where to write.
///
/// The one place iOS is easier than Android: a bundle's resources are
/// already real files on disk, so nothing has to be copied out before
/// SQLite can seek within it. An APK is a zip, and every asset there
/// costs a copy on first run.
#[cfg(not(target_vendor = "apple"))]
fn open_bundled_sources() -> Sources {
    // Off-Apple there is no bundle, no CoreLocation and no audio
    // session. Nothing here runs; it exists so the event loop, the touch
    // handling and the letterboxing below are type-checked on a Linux
    // laptop rather than only on a Mac.
    Sources::default()
}

#[cfg(target_vendor = "apple")]
fn open_bundled_sources() -> Sources {
    let data_dir = paths::documents();
    if let Some(dir) = &data_dir {
        log::info!("settings and routes in {}", dir.display());
    }

    let charts = paths::resource("noaa.mbtiles").and_then(|path| {
        match rutter_charts::ChartSource::open(&path, &[]) {
            Ok(source) => Some(source),
            Err(e) => {
                log::error!("charts: {}: {e}", path.display());
                None
            }
        }
    });

    // Downloaded imagery lives in Documents and takes precedence over
    // anything bundled: it is the newer of the two by definition, since
    // the user fetched it.
    let imagery = data_dir
        .as_ref()
        .map(|dir| dir.join("imagery.mbtiles"))
        .filter(|path| path.exists())
        .or_else(|| paths::resource("imagery.mbtiles"))
        .and_then(|path| match rutter_charts::ImagerySource::open(&path) {
            Ok(source) => Some(source),
            Err(e) => {
                log::error!("imagery: {}: {e}", path.display());
                None
            }
        });

    let tides = paths::resource("tides.sqlite").and_then(|path| {
        match rutter_tides::TideStations::open(&path) {
            Ok(stations) => Some(stations),
            Err(e) => {
                log::error!("tides: {}: {e}", path.display());
                None
            }
        }
    });

    let weather = paths::resource("weather.bin")
        .and_then(|path| std::fs::read(&path).ok())
        .and_then(|bytes| rutter_app::Forecast::parse(&bytes).ok());

    // The GPS and the hold that keeps the app alive are the same
    // `CLLocationManager`: on iOS the background location session *is*
    // the keepalive. The app opens the GPS when it wants one, so the two
    // are joined by a shared slot rather than by a value — see
    // `location::opener`.
    let gps_slot = location::shared();

    Sources {
        charts,
        imagery,
        tides,
        weather,
        nmea: None,
        gps: Some(rutter_app::DeviceGps::new(location::opener(
            gps_slot.clone(),
        ))),
        fetcher: match rutter_fetch::Https::new() {
            Ok(https) => Some(Arc::new(https)),
            Err(e) => {
                log::error!("no tile fetcher: {e}");
                None
            }
        },
        siren: match siren::open() {
            Ok(alarm) => Some(Arc::new(alarm)),
            Err(e) => {
                // The one thing a silent alarm must never do quietly.
                log::error!("no siren: {e}");
                None
            }
        },
        keepalive: Some(Arc::new(keepalive::Background::new(gps_slot))),
        zone: Some(Arc::new(zone::System)),
        imagery_out: data_dir.as_ref().map(|dir| dir.join("imagery.mbtiles")),
        data_dir,
    }
}

/// How much bigger the text is than egui's desktop default.
const FONT_SCALE: f32 = 1.25;

/// How much bigger the controls are.
///
/// More than the text, and deliberately: a pointer is exact and a
/// fingertip is about nine millimetres across, so what has to grow is the
/// area you can hit rather than the words inside it.
const CONTROL_SCALE: f32 = 1.5;

/// The boat, as a multiple of the screen's own density.
const BOAT_SCALE: f32 = 1.3;

/// Frames to keep asking for the safe area before settling for none.
const INSET_TRIES: u8 = 120;

/// Make the interface a finger can hit. See the Android shim, which this
/// matches deliberately — the two touch platforms should feel the same.
fn size_for_fingers(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        for font in style.text_styles.values_mut() {
            font.size *= FONT_SCALE;
        }
        let spacing = &mut style.spacing;
        spacing.interact_size *= CONTROL_SCALE;
        spacing.button_padding *= CONTROL_SCALE;
        spacing.icon_width *= CONTROL_SCALE;
        spacing.icon_width_inner *= CONTROL_SCALE;
        spacing.icon_spacing *= FONT_SCALE;
        spacing.slider_rail_height *= CONTROL_SCALE;
        spacing.slider_width *= CONTROL_SCALE;
        spacing.item_spacing *= FONT_SCALE;
    });
}

#[derive(Default)]
struct Platform {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    egui: Option<EguiState>,
    app: App,

    /// Fingers currently down, by winit's touch id, with their positions.
    fingers: Vec<(u64, (f64, f64))>,
    /// Whether the panel has claimed the gesture in progress, or `None`
    /// while that is not yet known. See [`Platform::settle_gesture`].
    gesture: Option<bool>,

    /// The surface, in pixels, as winit last reported it.
    surface: (u32, u32),
    /// What the system has claimed of the edges, in pixels.
    insets: (u32, u32, u32, u32),
    /// Frames still to be spent looking for the safe area.
    inset_tries: u8,
    /// How far apart two fingers were last time, for the pinch.
    pinch_separation: Option<f64>,
}

/// egui's context, without winit's translator — the same decision as the
/// Android shim, and for the same reason: what is wanted is raw touches
/// turned into egui pointer events, not a mouse emulation.
struct EguiState {
    ctx: egui::Context,
    events: Vec<egui::Event>,
    pixels_per_point: f32,
}

impl Platform {
    /// The part of the surface the system has not claimed, in pixels.
    fn safe_area(&self) -> (u32, u32, u32, u32) {
        let (width, height) = self.surface;
        let (left, top, right, bottom) = self.insets;
        let x = left.min(width.saturating_sub(1));
        let y = top.min(height.saturating_sub(1));
        let w = width.saturating_sub(x + right).max(1);
        let h = height.saturating_sub(y + bottom).max(1);
        (x, y, w, h)
    }

    /// Point the chart and the view at the safe area.
    fn apply_safe_area(&mut self) {
        let (x, y, w, h) = self.safe_area();
        self.app.resize(w, h);
        let claimed = self.insets != (0, 0, 0, 0);
        if let Some(r) = self.renderer.as_mut() {
            r.set_letterbox(claimed.then_some((x, y, w, h)));
        }
    }

    /// Ask UIKit for the safe area, in pixels.
    ///
    /// UIKit answers in *points*; everything else here is in pixels, so
    /// the scale factor is applied once, here, rather than being carried
    /// around as a unit question. The opposite of Android, which reports
    /// insets in pixels already.
    #[cfg(target_vendor = "apple")]
    fn read_insets(&self) -> Option<(u32, u32, u32, u32)> {
        let mtm = objc2::MainThreadMarker::new()?;
        let got = insets::current(mtm)?;
        let scale = f64::from(self.egui.as_ref().map_or(1.0, |e| e.pixels_per_point));
        let to_px = |points: f64| (points * scale).round().max(0.0) as u32;
        Some((
            to_px(got.left),
            to_px(got.top),
            to_px(got.right),
            to_px(got.bottom),
        ))
    }

    #[cfg(not(target_vendor = "apple"))]
    fn read_insets(&self) -> Option<(u32, u32, u32, u32)> {
        None
    }

    fn redraw_soon(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Screen pixels to egui's points.
    fn to_points(&self, (x, y): (f64, f64)) -> egui::Pos2 {
        let scale = f64::from(self.egui.as_ref().map_or(1.0, |e| e.pixels_per_point));
        egui::pos2((x / scale) as f32, (y / scale) as f32)
    }

    /// Feed one touch to egui.
    ///
    /// The press is *always* offered. Whether the panel keeps the gesture
    /// is settled a frame later — see [`Platform::settle_gesture`].
    fn offer_to_panel(&mut self, touch: &Touch) {
        let position = self.to_points((touch.location.x, touch.location.y));
        let Some(egui) = self.egui.as_mut() else {
            return;
        };
        let modifiers = egui::Modifiers::default();
        match touch.phase {
            TouchPhase::Started => {
                // The move first, or egui has no idea where the pointer
                // is when the press arrives and the click lands wherever
                // it was last frame.
                egui.events.push(egui::Event::PointerMoved(position));
                egui.events.push(egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers,
                });
            }
            TouchPhase::Moved => egui.events.push(egui::Event::PointerMoved(position)),
            TouchPhase::Ended | TouchPhase::Cancelled => {
                egui.events.push(egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers,
                });
                egui.events.push(egui::Event::PointerGone);
            }
        }
    }

    /// Decide whether the gesture in progress belongs to the panel.
    ///
    /// Called once a pass has run with the press in it, which is the
    /// first moment egui can answer. Asking at the moment of the press
    /// gets the answer for a screen nobody was touching — which on
    /// Android made every control need two taps, because the first was
    /// judged not-for-the-panel and its release was never delivered.
    fn settle_gesture(&mut self) {
        if self.gesture.is_some() || self.fingers.is_empty() {
            return;
        }
        let Some(egui) = self.egui.as_ref() else {
            return;
        };
        self.gesture =
            Some(egui.ctx.egui_wants_pointer_input() || egui.ctx.egui_is_using_pointer());
    }

    fn handle_touch(&mut self, touch: &Touch) {
        let id = touch.id;
        let now = (touch.location.x, touch.location.y);

        match touch.phase {
            TouchPhase::Started => {
                self.fingers.push((id, now));
                if self.fingers.len() == 1 {
                    self.offer_to_panel(touch);
                    self.gesture = None;
                } else {
                    // A pinch is never the panel's.
                    self.gesture = Some(false);
                }
                // The press needs a frame of its own: egui matches a
                // release against a potential click recorded in an
                // earlier pass, so press and release in one pass set
                // focus and nothing else.
                self.redraw_soon();
            }

            TouchPhase::Moved => {
                let previous = self.fingers.iter().find(|(f, _)| *f == id).map(|(_, p)| *p);
                if let Some(slot) = self.fingers.iter_mut().find(|(f, _)| *f == id) {
                    slot.1 = now;
                }
                match (self.fingers.len() >= 2, self.gesture) {
                    (true, _) => self.pinch_moved(),
                    (false, Some(true)) => self.offer_to_panel(touch),
                    (false, Some(false)) => {
                        if let Some(last) = previous {
                            self.app.pan_pixels(now.0 - last.0, now.1 - last.1);
                        }
                    }
                    // Undecided, which lasts one frame at most. Doing
                    // nothing is right: panning now would drag the chart
                    // out from under a slider the finger had landed on.
                    (false, None) => {}
                }
                self.redraw_soon();
            }

            TouchPhase::Ended | TouchPhase::Cancelled => {
                // Unconditionally, where the press went. egui needs the
                // release to turn a press into a click, and withholding
                // it leaves the widget stuck looking pressed.
                if self.fingers.len() == 1 {
                    self.offer_to_panel(touch);
                }
                self.fingers.retain(|(f, _)| *f != id);
                if self.fingers.is_empty() {
                    self.gesture = None;
                }
                self.redraw_soon();
            }
        }
    }

    /// Zoom by however much two fingers have spread.
    fn pinch_moved(&mut self) {
        let [(_, a), (_, b)] = match self
            .fingers
            .get(..2)
            .and_then(|s| <[_; 2]>::try_from(s).ok())
        {
            Some(pair) => pair,
            None => return,
        };
        let separation = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
        let centre = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        if let Some(previous) = self.pinch_separation {
            let change = separation - previous;
            if change.abs() >= 4.0 {
                // Into the chart's own pixels: a touch is reported
                // against the surface, and the chart starts below the
                // notch.
                let (left, top, _, _) = self.safe_area();
                self.app.zoom_about(
                    change / 60.0,
                    centre.0 - f64::from(left),
                    centre.1 - f64::from(top),
                );
                self.pinch_separation = Some(separation);
            }
        } else {
            self.pinch_separation = Some(separation);
        }
    }

    fn redraw(&mut self) {
        if self.inset_tries > 0 && self.insets == (0, 0, 0, 0) {
            self.inset_tries -= 1;
            if let Some(got) = self.read_insets().filter(|&i| i != (0, 0, 0, 0)) {
                log::info!("safe area insets: {got:?}");
                self.insets = got;
                self.apply_safe_area();
            }
        }

        if let Some(renderer) = self.renderer.as_mut() {
            self.app.refresh(renderer);
        }

        let pixels_per_point = self.egui.as_ref().map_or(1.0, |e| e.pixels_per_point);
        let state = self.renderer.as_ref().map(|r| {
            let mut state = self.app.panel_state(r, pixels_per_point);
            state.safe = (self.insets != (0, 0, 0, 0)).then(|| {
                let (x, y, w, h) = self.safe_area();
                let scale = pixels_per_point.max(f32::EPSILON);
                egui::Rect::from_min_size(
                    egui::pos2(x as f32 / scale, y as f32 / scale),
                    egui::vec2(w as f32 / scale, h as f32 / scale),
                )
            });
            state
        });

        let mut actions = Vec::new();
        let gui_output = match (self.egui.as_mut(), state.as_ref()) {
            (Some(egui), Some(state)) => {
                let input = egui::RawInput {
                    // The whole surface, not the safe area: egui's
                    // vertices have to land in surface pixels so it can
                    // paint the margins. Where controls may go is
                    // `PanelState::safe`.
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(
                            self.surface.0 as f32 / egui.pixels_per_point,
                            self.surface.1 as f32 / egui.pixels_per_point,
                        ),
                    )),
                    events: std::mem::take(&mut egui.events),
                    ..Default::default()
                };
                egui.ctx.set_pixels_per_point(egui.pixels_per_point);
                let app = &mut self.app;
                Some(egui.ctx.run_ui(input, |ui| {
                    actions = app.show(ui, state);
                }))
            }
            _ => None,
        };

        self.settle_gesture();

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

        if changed || wants_repaint {
            self.redraw_soon();
        }

        match frame {
            Frame::Drawn | Frame::Skipped => {}
            Frame::Reconfigured => self.redraw_soon(),
            // Routine on a phone rather than exceptional: it is what the
            // system does on the way to the background. `resumed` builds
            // a new one.
            Frame::SurfaceLost => {
                log::info!("surface lost; waiting for resume");
                self.renderer = None;
                self.window = None;
                self.app.forget_uploads();
            }
        }
    }
}

impl ApplicationHandler for Platform {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attributes = Window::default_attributes().with_title("rutter");
        let window = match event_loop.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("could not create the window: {e}");
                return;
            }
        };

        let size = window.inner_size();
        let pixels_per_point = window.scale_factor() as f32;
        log::info!(
            "surface {}x{} at {pixels_per_point} px/pt",
            size.width,
            size.height
        );
        self.surface = (size.width, size.height);

        let renderer =
            match pollster::block_on(Renderer::new(window.clone(), size.width, size.height)) {
                Ok(r) => r,
                Err(e) => {
                    log::error!("could not start the renderer: {e}");
                    return;
                }
            };
        self.renderer = Some(renderer);

        let ctx = egui::Context::default();
        size_for_fingers(&ctx);
        if let Some(renderer) = self.renderer.as_mut() {
            // The hull is laid out in physical pixels, and this screen
            // has `pixels_per_point` of them per point.
            renderer.set_boat_scale(pixels_per_point * BOAT_SCALE);
        }
        self.egui = Some(EguiState {
            ctx,
            events: Vec::new(),
            pixels_per_point,
        });

        self.window = Some(window);
        self.inset_tries = INSET_TRIES;
        self.apply_safe_area();
        self.redraw_soon();
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // Both must go, and in this order: the surface borrows the
        // window, so keeping the renderer across a suspend is a
        // use-after-free waiting for the next resume.
        log::info!("suspending; releasing the surface");
        self.renderer = None;
        self.window = None;
        self.fingers.clear();
        self.gesture = None;
        self.pinch_separation = None;
        self.app.forget_uploads();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                self.surface = (size.width, size.height);
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                // A rotation changes all four insets, so a known-good
                // answer is re-asked for here and the retry budget is
                // reset.
                self.inset_tries = INSET_TRIES;
                if let Some(got) = self.read_insets() {
                    if got != self.insets {
                        log::info!("safe area insets: {got:?}");
                        self.insets = got;
                    }
                }
                self.apply_safe_area();
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(egui) = self.egui.as_mut() {
                    egui.pixels_per_point = scale_factor as f32;
                }
            }

            WindowEvent::Touch(touch) => self.handle_touch(&touch),

            WindowEvent::RedrawRequested => self.redraw(),

            _ => {}
        }
    }
}
