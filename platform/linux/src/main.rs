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

use rutter_core::{chart, LatLon, View};
use rutter_render::{Frame, Renderer};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

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

    let mut app = App::default();
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("rutter: {e}");
        std::process::exit(1);
    }
}

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    view: View,
    /// Cursor position, kept so a drag can be measured.
    cursor: Option<(f64, f64)>,
    dragging: bool,
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

    fn redraw(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        match renderer.render(&self.view) {
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

        // pollster rather than tokio: this is the one await in the whole
        // program, and it happens once at startup.
        match pollster::block_on(Renderer::new(window.clone(), size.width, size.height)) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("rutter: {e}");
                std::process::exit(1);
            }
        }

        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
