//! Android board support.
//!
//! The mirror of `platform/linux`: same [`rutter_app`], same renderer,
//! different lifecycle and different input. The three differences that
//! matter are all here rather than leaking upward:
//!
//! * **The surface comes and goes.** On Linux a window exists from startup
//!   to exit. On Android the activity is told to release its surface
//!   whenever it is backgrounded, and gets a new one on return — so the
//!   renderer is built in `resumed` and dropped in `suspended`, and the app
//!   state outlives both. That is why `rutter-app` is handed the renderer
//!   rather than owning it.
//!
//! * **There is no `main`.** `android_main` is called by the NDK's glue
//!   after the JVM has started.
//!
//! * **There is no mouse and no scroll wheel.** One finger pans, two
//!   fingers pinch. Without the pinch there is no way to change zoom at all
//!   on a phone, which is how this shipped before.
//!
//! The data files come from the APK's asset store rather than a command
//! line, and have to be copied out before SQLite can open them.

// Off-Android, nothing reaches `Platform` because `android_main` is the only
// caller and it is compiled on Android alone. It is still *built* everywhere
// on purpose — type-checking this file on the laptop, without waiting for a
// cross-compile, is most of the value of having it in Rust at all.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

#[cfg(target_os = "android")]
use winit::platform::android::activity::AndroidApp;

/// Assets bundled in the APK, as `rutter apk` assembles them.
///
/// Named here and nowhere else, so the driver script and this file cannot
/// disagree about what is in the package.
const CHART_ASSET: &str = "noaa.mbtiles";
const IMAGERY_ASSET: &str = "imagery.mbtiles";
const TIDES_ASSET: &str = "tides.sqlite";
const WEATHER_ASSET: &str = "weather.bin";
/// One line naming a GPS feed, as `--nmea` takes on Linux.
///
/// A file because there is no command line here, and because the panel's
/// address box needs a soft keyboard that this shim does not yet raise —
/// so on Android the address has to arrive with the package. Written by
/// `rutter apk` from `RUTTER_NMEA`, and absent in a package built without
/// one, which is the normal case.
const NMEA_ASSET: &str = "nmea.txt";

// The phone's own GPS, which is Java and therefore JNI. Android-only, and
// the only file in the project that calls into the framework.
#[cfg(target_os = "android")]
mod gps;

// Keeping the process alive while an alarm is armed. The only reason
// there is any Java in this app at all.
// What the system has already taken of the screen — cutout, status
// bar, gesture bar. See the note there on edge-to-edge enforcement.
#[cfg(target_os = "android")]
mod insets;

/// Off-Android there is no window manager to ask, and `Platform` still
/// has to compile — see the note at the top of this file.
#[cfg(not(target_os = "android"))]
mod insets {
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Insets {
        pub left: u32,
        pub top: u32,
        pub right: u32,
        pub bottom: u32,
    }

    impl Insets {
        pub fn any(self) -> bool {
            false
        }
    }

    pub fn current() -> Option<Insets> {
        None
    }
}
#[cfg(target_os = "android")]
mod jvm;
#[cfg(target_os = "android")]
mod keepalive;
#[cfg(target_os = "android")]
mod siren;
#[cfg(target_os = "android")]
mod watch;

// The only unsafe thing in the project, and unavoidable: the NDK's glue
// finds this entry point by symbol name, so it cannot be mangled. Spelled
// with the explicit `unsafe(…)` form and an `allow` rather than relaxing the
// workspace lint, so the one exception stays visible in review.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    use winit::event_loop::EventLoop;
    use winit::platform::android::EventLoopBuilderExtAndroid;

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("rutter"),
    );

    log::info!("rutter starting");

    // Before anything that might want it. `ndk_context` holds the
    // `Application`, which is a `Context` and so answers nearly every
    // call this app makes — but `requestPermissions` is declared on
    // `Activity` alone, and without this it fails with
    // `NoSuchMethodError`. See `jvm.rs`.
    jvm::remember_activity(app.activity_as_ptr());

    // The assets must come off the store before the event loop takes the
    // AndroidApp.
    let sources = open_bundled_sources(&app);

    let event_loop = match EventLoop::builder().with_android_app(app).build() {
        Ok(el) => el,
        Err(e) => {
            log::error!("could not build the event loop: {e}");
            return;
        }
    };

    let mut platform = Platform {
        // No draft: the under-keel check is switched on from the panel, the
        // same as on a desktop with no `--draft`.
        app: App::new(sources, None),
        inset_tries: INSET_TRIES,
        ..Platform::default()
    };
    if let Err(e) = event_loop.run_app(&mut platform) {
        log::error!("event loop ended: {e}");
    }
}

/// Copy one asset out of the APK into internal storage, returning its path.
///
/// The copy is not avoidable. An asset is a zip entry inside the APK, and
/// SQLite needs a real file it can seek within — so the first run pays for
/// one copy per file and every run after it finds them already there.
///
/// Returns `None` on any failure, and logs why. A missing chart should leave
/// the app drawing an empty grid, not refuse to start: on a boat, an app
/// that opens with no chart is still a compass, and one that will not open
/// is nothing.
#[cfg(target_os = "android")]
fn extract_asset(app: &AndroidApp, name: &str) -> Option<std::path::PathBuf> {
    use std::ffi::CString;
    use std::io::Read;

    let dir = app.internal_data_path()?;
    let path = dir.join(name);

    if path.exists() {
        return Some(path);
    }

    log::info!("extracting {name} to {}", path.display());

    let c_name = CString::new(name).ok()?;
    let mut asset = match app.asset_manager().open(&c_name) {
        Some(a) => a,
        None => {
            // Not an error: imagery and tides are optional, and an APK
            // built without them should still run.
            log::info!("{name} is not in the package");
            return None;
        }
    };

    let mut bytes = Vec::new();
    if let Err(e) = asset.read_to_end(&mut bytes) {
        log::error!("reading asset {name}: {e}");
        return None;
    }

    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::error!("creating {}: {e}", dir.display());
        return None;
    }

    // Write via a temporary name and rename, so a run interrupted mid-copy
    // does not leave a truncated database that then looks present and fails
    // to open on every launch afterwards.
    let partial = path.with_extension("partial");
    if let Err(e) = std::fs::write(&partial, &bytes) {
        log::error!("writing {}: {e}", partial.display());
        return None;
    }
    if let Err(e) = std::fs::rename(&partial, &path) {
        log::error!("renaming into place: {e}");
        return None;
    }
    log::info!("extracted {name}, {} bytes", bytes.len());
    Some(path)
}

/// Open everything the APK carries.
#[cfg(target_os = "android")]
fn open_bundled_sources(app: &AndroidApp) -> Sources {
    let charts =
        extract_asset(app, CHART_ASSET).and_then(|path| match ChartSource::open(&path, &[]) {
            Ok(source) => {
                let meta = source.metadata();
                log::info!(
                    "charts: {} zoom {}..{}, {} layers",
                    if meta.name.is_empty() {
                        CHART_ASSET
                    } else {
                        &meta.name
                    },
                    meta.min_zoom,
                    meta.max_zoom,
                    meta.layers.len()
                );
                Some(source)
            }
            Err(e) => {
                log::error!("opening {}: {e}", path.display());
                None
            }
        });

    let imagery =
        extract_asset(app, IMAGERY_ASSET).and_then(|path| match ImagerySource::open(&path) {
            Ok(source) => {
                let meta = source.metadata();
                log::info!("imagery: zoom {}..{}", meta.min_zoom, meta.max_zoom);
                Some(source)
            }
            Err(e) => {
                log::error!("opening {}: {e}", path.display());
                None
            }
        });

    let tides = extract_asset(app, TIDES_ASSET).and_then(|path| match TideStations::open(&path) {
        Ok(source) => {
            log::info!("tides: {} stations", source.count().unwrap_or(0));
            Some(source)
        }
        Err(e) => {
            log::error!("opening {}: {e}", path.display());
            None
        }
    });

    // A forecast expires, unlike the chart and the tide constituents, so a
    // bundled one is only ever a starting point — the app says how old it is
    // and in-app fetching is what will really serve this.
    let weather = extract_asset(app, WEATHER_ASSET).and_then(|path| match std::fs::read(&path) {
        Ok(bytes) => match rutter_app::Forecast::parse(&bytes) {
            Ok(forecast) => {
                log::info!(
                    "weather: GFS run {}, {} hours",
                    forecast.run,
                    forecast.steps.len()
                );
                Some(forecast)
            }
            Err(e) => {
                log::error!("{}: {e}", path.display());
                None
            }
        },
        Err(e) => {
            log::error!("reading {}: {e}", path.display());
            None
        }
    });

    // Read rather than extracted-and-kept like the others: it is one
    // short line, and a stale copy in the data directory would quietly
    // override a new address in a reinstalled package.
    let nmea = extract_asset(app, NMEA_ASSET)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| match rutter_app::NmeaEndpoint::parse(&text) {
            Ok(endpoint) => {
                log::info!("nmea: {} {}", endpoint.protocol_name(), endpoint.address());
                Some(endpoint)
            }
            Err(e) => {
                log::error!("{NMEA_ASSET}: {e}");
                None
            }
        });

    Sources {
        charts,
        imagery,
        tides,
        weather,
        nmea,
        // The phone's own GPS. A factory rather than an opened source so
        // that the app can switch to a network feed and back again — and
        // so the permission dialogue comes up when the GPS is wanted
        // rather than while the charts are still being extracted.
        //
        // Used automatically when no `nmea.txt` named a feed, which is the
        // normal case: a phone's own GPS is the obvious position source,
        // and the panel can still be pointed at the boat's instruments.
        gps: Some(rutter_app::DeviceGps::new(gps::open)),
        fetcher: match rutter_fetch::Https::new() {
            Ok(https) => Some(std::sync::Arc::new(https)),
            Err(e) => {
                log::error!("no tile fetcher: {e}");
                None
            }
        },
        // Downloads go into the app's own data directory — the one place a
        // sandboxed app may write.
        //
        // The same file the bundled imagery was extracted to, because a
        // download now *adds* to what is there rather than replacing it:
        // fetching a harbour tops up the coverage the package shipped
        // with instead of competing with it.
        imagery_out: app.internal_data_path().map(|dir| dir.join(IMAGERY_ASSET)),
        // The same directory as everything else here. A sandboxed app has
        // exactly one place it may write, and it is wiped with the app —
        // which is the right lifetime for a draft and a planned passage.
        // On the alarm stream, so a silenced phone still sounds it —
        // see `siren.rs`. If it will not open, the alarms still watch and
        // still draw and the panel says plainly that they cannot be
        // heard, which is the one thing a silent alarm must never do
        // quietly.
        siren: match siren::Tones::open() {
            Ok(tones) => Some(std::sync::Arc::new(tones)),
            Err(e) => {
                log::error!("no siren: {e}");
                None
            }
        },
        // The foreground service, so an anchor watch survives the screen
        // going off.
        keepalive: Some(std::sync::Arc::new(keepalive::Service)),
        data_dir: app.internal_data_path(),
    }
}

// Everything below is target-independent, so that `cargo check` on the
// laptop still type-checks it.

use std::sync::Arc;

use rutter_app::App;
use rutter_render::{Frame, GuiFrame, Renderer};

// Only the asset-loading code above touches these, and that is Android-only,
// so off-Android they really are unused rather than conditionally used.
#[cfg(target_os = "android")]
use rutter_app::Sources;
#[cfg(target_os = "android")]
use rutter_charts::{ChartSource, ImagerySource};
#[cfg(target_os = "android")]
use rutter_tides::TideStations;
use winit::application::ApplicationHandler;
use winit::event::{Touch, TouchPhase, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

/// Pixels of pinch separation per zoom step.
///
/// A zoom step is a quarter of a zoom level, so this is 160 px of finger
/// travel per doubling — about one comfortable thumb-and-finger spread on a
/// phone. Guessed values here are exactly what needs a device to tune, and
/// this one was tuned on hardware rather than invented.
const PINCH_PIXELS_PER_STEP: f64 = 40.0;

/// Two fingers, as the gesture tracker sees them.
#[derive(Debug, Clone, Copy)]
struct Pinch {
    /// Distance between the fingers when the gesture last produced a zoom.
    separation: f64,
    /// Midpoint, which is what the zoom is anchored about.
    centre: (f64, f64),
}

/// Frames to keep asking for the insets before settling for none.
/// Roughly two seconds at sixty frames a second.
const INSET_TRIES: u8 = 120;

#[derive(Default)]
struct Platform {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    egui: Option<EguiState>,
    app: App,

    /// Fingers currently down, by winit's touch id, with their positions.
    ///
    /// A Vec rather than a map: there are at most a handful, and the order
    /// they went down in is the order to treat them as first and second.
    fingers: Vec<(u64, (f64, f64))>,
    /// The pinch in progress, if two fingers are down.
    pinch: Option<Pinch>,
    /// Whether the panel has had the current gesture. egui gets the first
    /// refusal on a touch, the same as it gets on a Linux click.
    gesture_consumed: bool,

    /// The surface, in pixels, as winit last reported it.
    ///
    /// Kept because the safe area is derived from it and from the system
    /// insets, and the two arrive separately.
    surface: (u32, u32),
    /// What the system has claimed of the edges. See `insets.rs`.
    insets: insets::Insets,
    /// Frames still to be spent looking for the insets.
    ///
    /// `getRootWindowInsets` returns null until the decor view is
    /// attached, which is routinely still true at the first `Resized` —
    /// and nothing guarantees a second one. Without a retry the app would
    /// settle on "no insets" and draw under the cutout for the rest of
    /// the run. Bounded because a device with genuinely nothing claimed
    /// is a real answer, and asking forever would be a handful of JNI
    /// calls every frame to learn zero again.
    inset_tries: u8,
}

/// egui's context, without winit's translator.
///
/// `egui-winit` exists and works on Android, but it is not used here: it
/// wants a window to read a scale factor and a clipboard from, and what this
/// shim needs is the opposite direction — raw touches turned into egui
/// pointer events so that one finger can both drive a checkbox and pan a
/// chart. Doing that by hand is a dozen lines and avoids guessing at how
/// egui-winit maps a multi-touch sequence.
struct EguiState {
    ctx: egui::Context,
    /// Events gathered since the last frame.
    events: Vec<egui::Event>,
    /// Where egui thinks the pointer is.
    pointer: egui::Pos2,
    pixels_per_point: f32,
}

impl Platform {
    /// The part of the surface the system has not claimed, in pixels.
    ///
    /// Clamped so that a bad or enormous inset cannot produce a zero or
    /// negative viewport, which wgpu rejects outright.
    fn safe_area(&self) -> (u32, u32, u32, u32) {
        let (width, height) = self.surface;
        let x = self.insets.left.min(width.saturating_sub(1));
        let y = self.insets.top.min(height.saturating_sub(1));
        let w = width.saturating_sub(x + self.insets.right).max(1);
        let h = height.saturating_sub(y + self.insets.bottom).max(1);
        (x, y, w, h)
    }

    /// Point the chart and the view at the safe area.
    ///
    /// The view is sized to the safe area rather than the surface, so
    /// that centring on the boat puts it in the middle of what can
    /// actually be seen rather than in the middle of the glass — where,
    /// with a 152-pixel cutout above and a gesture bar below, it would
    /// sit visibly low.
    fn apply_safe_area(&mut self) {
        let (x, y, w, h) = self.safe_area();
        self.app.resize(w, h);
        if let Some(r) = self.renderer.as_mut() {
            r.set_letterbox(self.insets.any().then_some((x, y, w, h)));
        }
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

    /// Feed one touch to egui, and report whether egui wants it.
    ///
    /// Returns true when the panel is using this gesture, in which case the
    /// chart must not also pan — otherwise dragging the imagery slider
    /// would drag the chart under it.
    fn offer_to_panel(&mut self, touch: &Touch) -> bool {
        let position = self.to_points((touch.location.x, touch.location.y));
        let Some(egui) = self.egui.as_mut() else {
            return false;
        };

        egui.pointer = position;
        let modifiers = egui::Modifiers::default();
        match touch.phase {
            TouchPhase::Started => {
                // A press needs the move first, or egui has no idea where
                // the pointer is when the press arrives and the click lands
                // wherever the pointer was last frame.
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

        // `is_using_pointer` covers a drag in progress; `wants_pointer_input`
        // covers the frame a widget is first touched.
        egui.ctx.egui_wants_pointer_input() || egui.ctx.egui_is_using_pointer()
    }

    /// Update the finger list and act on what the gesture became.
    fn handle_touch(&mut self, touch: &Touch) {
        let id = touch.id;
        let now = (touch.location.x, touch.location.y);

        match touch.phase {
            TouchPhase::Started => {
                self.fingers.push((id, now));
                if self.fingers.len() == 1 {
                    // Only the first finger of a gesture is offered to the
                    // panel. A second finger means a pinch, which the panel
                    // has no use for.
                    self.gesture_consumed = self.offer_to_panel(touch);
                }
                self.pinch = self.start_pinch();
                // The press needs a frame of its own.
                //
                // egui decides a widget was clicked by matching the
                // release against a *potential click* recorded when the
                // press was last laid out — which is to say, in an
                // earlier pass. Deliver press and release in the same
                // pass and there is no potential click yet to match, so
                // the widget takes focus and does nothing else.
                //
                // On hardware that read as a dead interface: every
                // button needed a second, unrelated tap before the first
                // one took effect, and a tap on its own looked ignored.
                // A mouse never showed it because a desktop redraws on
                // the button-down as well.
                self.redraw_soon();
            }

            TouchPhase::Moved => {
                let previous = self.fingers.iter().find(|(f, _)| *f == id).map(|(_, p)| *p);
                if let Some(slot) = self.fingers.iter_mut().find(|(f, _)| *f == id) {
                    slot.1 = now;
                }

                if self.fingers.len() >= 2 {
                    self.pinch_moved();
                } else if self.gesture_consumed {
                    self.offer_to_panel(touch);
                } else if let Some(last) = previous {
                    self.app.pan_pixels(now.0 - last.0, now.1 - last.1);
                }
                self.redraw_soon();
            }

            TouchPhase::Ended | TouchPhase::Cancelled => {
                if self.gesture_consumed && self.fingers.len() == 1 {
                    self.offer_to_panel(touch);
                }
                self.fingers.retain(|(f, _)| *f != id);
                // Lifting one finger of a pinch must re-baseline rather
                // than leave a stale separation, or the chart jumps when
                // the remaining finger moves.
                self.pinch = self.start_pinch();
                if self.fingers.is_empty() {
                    self.gesture_consumed = false;
                }
                self.redraw_soon();
            }
        }
    }

    /// The current two-finger state, if there is one.
    fn start_pinch(&self) -> Option<Pinch> {
        let [(_, a), (_, b)] = self.fingers.get(..2)?.try_into().ok()?;
        Some(Pinch {
            separation: distance(a, b),
            centre: ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0),
        })
    }

    /// Zoom by however much the fingers have spread since the last step.
    fn pinch_moved(&mut self) {
        let Some(previous) = self.pinch else {
            self.pinch = self.start_pinch();
            return;
        };
        let Some(current) = self.start_pinch() else {
            return;
        };

        let change = current.separation - previous.separation;
        // A dead zone, so that resting two fingers does not drift the zoom.
        if change.abs() < 4.0 {
            return;
        }

        // Into the chart's own pixels: a touch is reported against the
        // surface, and the chart now starts below the cutout.
        let (left, top, _, _) = self.safe_area();
        self.app.zoom_about(
            change / PINCH_PIXELS_PER_STEP,
            previous.centre.0 - f64::from(left),
            previous.centre.1 - f64::from(top),
        );
        self.pinch = Some(current);
    }

    fn redraw(&mut self) {
        if self.inset_tries > 0 && !self.insets.any() {
            self.inset_tries -= 1;
            if let Some(got) = insets::current().filter(|got| got.any()) {
                log::info!("insets: {got:?}");
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
            // Where the panel may lay itself out. The app cannot know
            // this — it is a fact about the glass, not about charts.
            state.safe = self.insets.any().then(|| {
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
                    // The whole surface, not the safe area. egui's
                    // vertices have to land in surface pixels so that it
                    // can paint the margins black; where it may put
                    // *controls* is `PanelState::safe`, which it applies
                    // itself. Not `view.width_px` either — that is the
                    // chart's size now, which is the safe area.
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
                let output = egui.ctx.run_ui(input, |ui| {
                    actions = app.show(ui, state);
                });
                Some(output)
            }
            _ => None,
        };

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
            // Routine here, unlike on Linux: it is what Android does on the
            // way out. `resumed` builds a new one.
            Frame::SurfaceLost => {
                log::info!("surface lost; waiting for resume");
                self.renderer = None;
                self.window = None;
                self.app.forget_uploads();
            }
        }
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    (dx * dx + dy * dy).sqrt()
}

impl ApplicationHandler for Platform {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = match event_loop.create_window(Window::default_attributes()) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("no window: {e}");
                return;
            }
        };

        let size = window.inner_size();
        self.app.resize(size.width, size.height);

        // Open on the chart on first launch only: coming back from the
        // background should return to where the user was.
        if self.egui.is_none() {
            self.app.fit_chart();
        }

        match pollster::block_on(Renderer::new(window.clone(), size.width, size.height)) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                log::error!("no renderer: {e}");
                return;
            }
        }

        // Android reports its density as the window's scale factor. egui
        // needs it or the panel is drawn at a quarter size on a phone and
        // the touch targets land nowhere near the widgets.
        let pixels_per_point = window.scale_factor() as f32;
        log::info!(
            "surface {}x{} at {pixels_per_point} px/pt",
            size.width,
            size.height
        );

        let ctx = egui::Context::default();
        self.egui = Some(EguiState {
            ctx,
            events: Vec::new(),
            pointer: egui::Pos2::ZERO,
            pixels_per_point,
        });

        self.window = Some(window);
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // Both must go, and in this order: the surface borrows the window, so
        // keeping the renderer across a suspend is a use-after-free waiting
        // for the next resume.
        log::info!("suspending; releasing the surface");
        self.renderer = None;
        self.window = None;
        self.fingers.clear();
        self.pinch = None;
        self.gesture_consumed = false;
        // The uploaded geometry went with the renderer. The sources and
        // their decoded-tile caches survive, so coming back costs an upload
        // but not a re-decode.
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
                // Asked for again on every resize rather than cached at
                // startup: a cutout is a different size on every phone,
                // the gesture bar differs between navigation modes, and
                // all four edges change on rotation. It is also null
                // until the view is attached, so the first query during
                // startup commonly fails and this is where it succeeds.
                // A rotation changes all four, so a known-good answer
                // is re-asked for here too — and the retry budget is
                // reset, since a resize is exactly when the old answer
                // stops being true.
                self.inset_tries = INSET_TRIES;
                if let Some(got) = insets::current() {
                    if got != self.insets {
                        log::info!("insets: {got:?}");
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
