//! Chart rendering, via wgpu.
//!
//! wgpu is this project's hardware abstraction layer, and it is the reason
//! the whole thing is Rust. One renderer, written once, runs on Vulkan
//! (Android, Linux), Metal (iOS, macOS), D3D12 (Windows) and GL ES as a
//! fallback. The alternative — a GL 2.1 path for desktop and a GL ES 2 path
//! for mobile, which is what OpenCPN does — is two renderers that drift.
//!
//! This crate is portable in source but not testable in CI: it needs a GPU
//! and a surface. The split is deliberate. Anything that can be checked
//! without a screen belongs in `rutter-core`, which is why the projection
//! and tile maths are not here.
//!
//! What it draws today is the chart *grid*: the boundaries of the tiles the
//! view needs, plus a mark at the centre. That is not a chart, but it is the
//! whole pipeline — core computes geography, the renderer puts it on a GPU
//! surface, and panning and zooming are visibly correct or visibly not.
//! Tile imagery goes on top of this without changing its shape.

use rutter_core::chart::{self, TileId};
use rutter_core::View;

/// Sea. Everything is drawn over this.
const SEA: wgpu::Color = wgpu::Color {
    r: 0.043,
    g: 0.149,
    b: 0.231,
    a: 1.0,
};

/// Tile boundaries.
const GRID_COLOUR: [f32; 4] = [0.25, 0.42, 0.55, 1.0];

/// The mark at the view centre — where the boat would be.
const MARK_COLOUR: [f32; 4] = [0.95, 0.75, 0.15, 1.0];

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    /// Offset from the view centre, in normalised world units.
    offset: [f32; 2],
    colour: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    scale: [f32; 2],
    pixels_per_world: f32,
    _pad: f32,
}

/// How many vertices the geometry buffer can hold before it is reallocated.
///
/// A phone screen at any zoom needs a few hundred tiles at most, each
/// contributing eight vertices for four edges. 64k is generous and costs
/// 1.5 MB.
const VERTEX_CAPACITY: u64 = 65_536;

#[derive(Debug)]
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    vertices: wgpu::Buffer,
    vertex_count: u32,
    /// Scratch, kept between frames so a redraw does not allocate.
    scratch: Vec<Vertex>,
}

#[derive(Debug)]
pub enum Error {
    NoAdapter,
    Surface(wgpu::CreateSurfaceError),
    Device(wgpu::RequestDeviceError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter => write!(
                f,
                "no usable GPU adapter — on a headless or driverless machine, \
                 install a Vulkan driver or Mesa's llvmpipe"
            ),
            Self::Surface(e) => write!(f, "could not create a surface: {e}"),
            Self::Device(e) => write!(f, "could not open the GPU device: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// What came of asking for a frame.
///
/// Deliberately this crate's own enum rather than wgpu's, so the platform
/// shims above do not need to depend on wgpu at all — their whole job is the
/// window and the lifecycle, and a board support package that has opinions
/// about the graphics API is doing too much.
///
/// Every variant except `Drawn` is normal operation, not an error. A surface
/// going outdated on a resize or lost on an Android background is routine,
/// and the renderer handles both itself; the caller only needs to know
/// whether to ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// Drawn and presented.
    Drawn,
    /// Nothing drawn, and nothing wrong — the window is occluded or the
    /// compositor timed out. Try again on the next redraw.
    Skipped,
    /// The surface was reconfigured and nothing was drawn. Ask for another
    /// redraw immediately.
    Reconfigured,
    /// The surface is gone and cannot be rebuilt from here. The platform has
    /// to recreate the renderer from its window — which on Android is what
    /// happens on every return to the foreground.
    SurfaceLost,
}

impl Renderer {
    /// Bring up a renderer on `target`, which is a window the caller keeps
    /// alive for at least as long as this renderer.
    pub async fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
    ) -> Result<Self, Error> {
        // All backends: let wgpu pick Vulkan on Linux and Android, Metal on
        // Apple, D3D12 on Windows, and fall back to GL ES where it must.
        //
        // Without a display handle, by necessity: wgpu 30 takes one as
        // `Box<dyn WgpuHasDisplayHandle>` and does not re-export that trait,
        // so naming the type here would mean depending on `wgpu-types`
        // directly. It is only consulted by the GL ES backend, and mainly on
        // Wayland — so the cost is that the GL ES *fallback* may fail to
        // present on a Wayland session with no Vulkan driver at all. Both
        // real targets here use Vulkan, and Mesa's lavapipe covers the
        // driverless case in software.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

        let surface = instance.create_surface(target).map_err(Error::Surface)?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                // LowPower rather than HighPerformance: this runs on a phone,
                // on battery, for a whole passage. A chart is not a game.
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                // Limit bucketing is an anti-fingerprinting measure for
                // hosts that expose wgpu to untrusted content, like a
                // browser. This is a native app; rounding the real limits
                // away would only lose information.
                apply_limit_buckets: false,
            })
            .await
            .map_err(|_| Error::NoAdapter)?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("rutter"),
                required_features: wgpu::Features::empty(),
                // downlevel_defaults, not the desktop defaults: these are the
                // limits GL ES 3.0 can promise, which is the floor on an old
                // Android device. Asking for more here is how a renderer
                // works on the development laptop and fails on the phone.
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: Default::default(),
            })
            .await
            .map_err(Error::Device)?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            // Auto: let the platform pick. An explicit wide-gamut or HDR
            // space is a decision to make once there is chart imagery whose
            // colours are worth arguing about.
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: width.max(1),
            height: height.max(1),
            // Fifo is the only mode guaranteed everywhere, and vsync is what
            // you want when the alternative is burning battery to render a
            // chart that is not moving.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("chart"),
            source: wgpu::ShaderSource::Wgsl(include_str!("chart.wgsl").into()),
        });

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("view"),
            size: std::mem::size_of::<ViewUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("view"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("view"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("chart"),
            bind_group_layouts: &[Some(&bind_layout)],
            // No immediate data. The view uniform is small enough that it
            // could live here instead of in a buffer, but immediate data has
            // a tighter size limit on GL ES and this is not a hot path.
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("chart"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x4,
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                // Lines, because everything drawn so far is an outline. When
                // tile imagery lands it gets its own pipeline rather than
                // sharing this one.
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("geometry"),
            size: VERTEX_CAPACITY * std::mem::size_of::<Vertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            uniform,
            bind_group,
            vertices,
            vertex_count: 0,
            scratch: Vec::with_capacity(4096),
        })
    }

    /// Current surface size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Reconfigure for a new surface size.
    ///
    /// A zero in either dimension means the window is minimised, or — on
    /// Android — that the activity is going away. Reconfiguring to zero is a
    /// validation error, so it is ignored.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Rebuild the surface configuration after it was lost — which happens
    /// routinely on Android when the app is backgrounded.
    pub fn reconfigure(&mut self) {
        self.surface.configure(&self.device, &self.config);
    }

    /// Draw one frame of `view`.
    pub fn render(&mut self, view: &View) -> Frame {
        use wgpu::CurrentSurfaceTexture as Acquired;

        self.build_geometry(view);
        self.upload(view);

        let frame = match self.surface.get_current_texture() {
            Acquired::Success(f) => f,
            // Suboptimal still gives a usable texture; draw into it and fix
            // the configuration for next time rather than dropping a frame.
            Acquired::Suboptimal(f) => {
                self.reconfigure();
                f
            }
            Acquired::Timeout | Acquired::Occluded => return Frame::Skipped,
            Acquired::Outdated => {
                self.reconfigure();
                return Frame::Reconfigured;
            }
            Acquired::Lost => return Frame::SurfaceLost,
            Acquired::Validation => {
                log::error!("surface validation error — this is a bug in the renderer");
                return Frame::Skipped;
            }
        };

        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("chart"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(SEA),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if self.vertex_count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.vertices.slice(..));
                pass.draw(0..self.vertex_count, 0..1);
            }
        }

        self.queue.submit(Some(encoder.finish()));
        // Presenting is the queue's job in wgpu 30, not the texture's.
        self.queue.present(frame);
        Frame::Drawn
    }

    /// World units visible per pixel, and the clip-space scale that follows.
    fn uniform_for(&self, view: &View) -> ViewUniform {
        // 256 pixels per tile, 2^zoom tiles across the world: this is the
        // definition of slippy-map zoom, and the fractional part is what
        // makes a pinch gesture smooth rather than stepped.
        let pixels_per_world = 256.0 * 2f64.powf(view.zoom);

        ViewUniform {
            scale: [
                (2.0 * pixels_per_world / self.config.width.max(1) as f64) as f32,
                (2.0 * pixels_per_world / self.config.height.max(1) as f64) as f32,
            ],
            pixels_per_world: pixels_per_world as f32,
            _pad: 0.0,
        }
    }

    fn upload(&mut self, view: &View) {
        self.queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&self.uniform_for(view)),
        );

        if self.vertex_count > 0 {
            self.queue
                .write_buffer(&self.vertices, 0, bytemuck::cast_slice(&self.scratch));
        }
    }

    /// Turn the view's tile list into line segments.
    fn build_geometry(&mut self, view: &View) {
        // Taken out of `self` for the duration so that the push helper below
        // can borrow it mutably while the length is also read. Swapped back
        // at the end, which keeps the allocation across frames — the point of
        // keeping it in `self` in the first place.
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();

        // Everything is relative to the view centre, so the numbers reaching
        // f32 stay small no matter where on the planet the boat is.
        let centre = chart::project(view.centre);

        let line = |out: &mut Vec<Vertex>, ax: f64, ay: f64, bx: f64, by: f64, colour| {
            out.push(Vertex {
                offset: [(ax - centre.x) as f32, (ay - centre.y) as f32],
                colour,
            });
            out.push(Vertex {
                offset: [(bx - centre.x) as f32, (by - centre.y) as f32],
                colour,
            });
        };

        let z = view.tile_zoom();
        let n = TileId::grid_size(z) as f64;

        for tile in view.visible_tiles() {
            // Leave room for the four edges plus the two centre-mark strokes
            // added after the loop.
            if out.len() as u64 + 12 > VERTEX_CAPACITY {
                // Rather than reallocate mid-frame, stop. A view needing this
                // many tiles is a bug in the zoom clamp, and a partial grid
                // makes that visible instead of merely slow.
                log::warn!("tile geometry hit the vertex cap at zoom {z}");
                break;
            }

            let (x0, y0) = (tile.x as f64 / n, tile.y as f64 / n);
            let (x1, y1) = ((tile.x + 1) as f64 / n, (tile.y + 1) as f64 / n);

            // Four edges. Drawn per tile rather than as a shared lattice:
            // redundant, it does not matter at a few hundred tiles, and it is
            // the shape that generalises to per-tile imagery.
            line(&mut out, x0, y0, x1, y0, GRID_COLOUR);
            line(&mut out, x1, y0, x1, y1, GRID_COLOUR);
            line(&mut out, x1, y1, x0, y1, GRID_COLOUR);
            line(&mut out, x0, y1, x0, y0, GRID_COLOUR);
        }

        // A mark at the centre, sized in world units so it stays the same
        // size on screen at every zoom.
        let arm = 12.0 / (256.0 * 2f64.powf(view.zoom));
        line(
            &mut out,
            centre.x - arm,
            centre.y,
            centre.x + arm,
            centre.y,
            MARK_COLOUR,
        );
        line(
            &mut out,
            centre.x,
            centre.y - arm,
            centre.x,
            centre.y + arm,
            MARK_COLOUR,
        );

        self.vertex_count = out.len() as u32;
        self.scratch = out;
    }
}
