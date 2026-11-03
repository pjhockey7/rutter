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
//! It draws two things, in two vertex buffers, for a reason worth stating.
//!
//! The *grid* — visible tile boundaries and a centre mark — is rebuilt every
//! frame, because it depends on the viewport and is a few hundred lines.
//!
//! The *chart* — real coastlines, depth contours and soundings — is uploaded
//! once per change of visible tile set and left alone. Its vertices are
//! stored relative to a reference point fixed at upload time, so a pan
//! changes one uniform rather than re-sending every coastline. That split is
//! the difference between a drag that costs four bytes and one that costs a
//! megabyte a frame.
//!
//! This crate depends on `rutter-core` and nothing else of the project's:
//! chart geometry arrives as plain slices of world coordinates, so reading
//! MBTiles — and linking SQLite — stays in `rutter-charts`, where a renderer
//! does not have to care about it.

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

/// Chart linework: coastlines, depth area boundaries, contours.
const CHART_COLOUR: [f32; 4] = [0.82, 0.90, 0.95, 1.0];

/// Point features — soundings, buoys, beacons.
const SOUNDING_COLOUR: [f32; 4] = [0.60, 0.85, 0.70, 1.0];

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    /// Offset from this buffer's reference point, in normalised world units.
    offset: [f32; 2],
    colour: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    scale: [f32; 2],
    /// This buffer's reference point relative to the view centre.
    origin: [f32; 2],
}

/// Vertices the grid buffer holds. Fixed, because the grid is bounded: a
/// viewport covers a few hundred tiles at most, eight vertices each for four
/// edges. 64k is generous and costs 1.5 MB.
const GRID_CAPACITY: u64 = 65_536;

/// One buffer's worth of geometry: the vertices and the point they are
/// measured from.
///
/// Pairing the two is the invariant that matters. Vertices are f32 offsets
/// and only mean anything against their own origin, so a buffer whose origin
/// is forgotten draws in the wrong place — subtly, and worse the further the
/// view has moved.
#[derive(Debug)]
struct Layer {
    vertices: wgpu::Buffer,
    /// Capacity in vertices, so growth is a decision rather than a surprise.
    capacity: u64,
    count: u32,
    /// The reference point these offsets are measured from, in f64 because
    /// the subtraction against the view centre must not lose precision
    /// before it reaches f32.
    origin: chart::WorldXY,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

#[derive(Debug)]
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    /// Tile boundaries and the centre mark. Rebuilt every frame.
    grid: Layer,
    /// Real chart geometry. Uploaded on change, then left alone.
    charts: Layer,
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

        let grid = Layer::new(&device, &bind_layout, "grid", GRID_CAPACITY);
        // The chart layer starts empty; `set_chart` sizes it to the data.
        let charts = Layer::new(&device, &bind_layout, "charts", 1);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_layout,
            grid,
            charts,
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

            pass.set_pipeline(&self.pipeline);

            // Grid first, chart over it: the grid is scaffolding and should
            // not sit on top of the coastline it is there to locate.
            for layer in [&self.grid, &self.charts] {
                if layer.count == 0 {
                    continue;
                }
                pass.set_bind_group(0, &layer.bind_group, &[]);
                pass.set_vertex_buffer(0, layer.vertices.slice(..));
                pass.draw(0..layer.count, 0..1);
            }
        }

        self.queue.submit(Some(encoder.finish()));
        // Presenting is the queue's job in wgpu 30, not the texture's.
        self.queue.present(frame);
        Frame::Drawn
    }

    /// Clip units per world unit, for this viewport and zoom.
    fn scale_for(&self, view: &View) -> [f32; 2] {
        // 256 pixels per tile, 2^zoom tiles across the world: this is the
        // definition of slippy-map zoom, and the fractional part is what
        // makes a pinch gesture smooth rather than stepped.
        let pixels_per_world = 256.0 * 2f64.powf(view.zoom);

        [
            (2.0 * pixels_per_world / self.config.width.max(1) as f64) as f32,
            (2.0 * pixels_per_world / self.config.height.max(1) as f64) as f32,
        ]
    }

    /// Refresh both layers' uniforms, and the grid's vertices.
    ///
    /// The chart's vertices are deliberately absent: they were uploaded when
    /// the tile set changed and are still correct, because they are measured
    /// from the chart layer's own origin rather than from the view.
    fn upload(&mut self, view: &View) {
        let scale = self.scale_for(view);
        let centre = chart::project(view.centre);

        for layer in [&self.grid, &self.charts] {
            // The subtraction happens in f64 and only the small difference
            // becomes f32. Projecting to f32 first would throw away the
            // precision this whole arrangement exists to keep.
            let uniform = ViewUniform {
                scale,
                origin: [
                    (layer.origin.x - centre.x) as f32,
                    (layer.origin.y - centre.y) as f32,
                ],
            };
            self.queue
                .write_buffer(&layer.uniform, 0, bytemuck::bytes_of(&uniform));
        }

        if self.grid.count > 0 {
            self.queue
                .write_buffer(&self.grid.vertices, 0, bytemuck::cast_slice(&self.scratch));
        }
    }

    /// Replace the chart geometry.
    ///
    /// Call this when the set of visible tiles changes, not every frame.
    /// `lines` is a polyline per entry and `points` are standalone features;
    /// both are in normalised world coordinates, which is all this crate
    /// needs to know about where chart data came from.
    ///
    /// The reference point is taken from the geometry's own bounding box
    /// rather than from the current view. That is what lets a pan leave this
    /// buffer untouched: tying the origin to the view would invalidate every
    /// vertex the moment the view moved.
    pub fn set_chart(&mut self, lines: &[&[chart::WorldXY]], points: &[chart::WorldXY]) {
        let mut vertices: Vec<Vertex> = Vec::new();

        let origin = centroid(lines, points);

        let offset = |p: chart::WorldXY| [(p.x - origin.x) as f32, (p.y - origin.y) as f32];

        for line in lines {
            // A polyline of n points is n-1 separate segments under
            // LineList. A line strip topology would halve this, but it
            // cannot express several disconnected polylines in one draw.
            for pair in line.windows(2) {
                vertices.push(Vertex {
                    offset: offset(pair[0]),
                    colour: CHART_COLOUR,
                });
                vertices.push(Vertex {
                    offset: offset(pair[1]),
                    colour: CHART_COLOUR,
                });
            }
        }

        // Soundings as small crosses, in world units scaled so they stay a
        // constant size on screen is not possible here — this buffer does
        // not know the zoom. Size them in world units relative to the data's
        // own extent instead, which keeps them visible at the zoom the chart
        // was loaded for.
        let arm = point_arm(lines, points);
        for &p in points {
            let c = offset(p);
            for (dx, dy) in [(arm, 0.0), (0.0, arm)] {
                vertices.push(Vertex {
                    offset: [c[0] - dx, c[1] - dy],
                    colour: SOUNDING_COLOUR,
                });
                vertices.push(Vertex {
                    offset: [c[0] + dx, c[1] + dy],
                    colour: SOUNDING_COLOUR,
                });
            }
        }

        self.charts.origin = origin;
        self.charts.count = vertices.len() as u32;

        if vertices.is_empty() {
            return;
        }

        // Grow rather than truncate. Chart density is not something this
        // crate can bound: one tile of a busy harbour carries more linework
        // than a hundred tiles of open sea.
        if vertices.len() as u64 > self.charts.capacity {
            let capacity = (vertices.len() as u64).next_power_of_two();
            log::debug!("growing chart buffer to {capacity} vertices");
            self.charts = Layer::new(&self.device, &self.bind_layout, "charts", capacity);
            self.charts.origin = origin;
            self.charts.count = vertices.len() as u32;
        }

        self.queue
            .write_buffer(&self.charts.vertices, 0, bytemuck::cast_slice(&vertices));
    }

    /// Drop the chart geometry.
    pub fn clear_chart(&mut self) {
        self.charts.count = 0;
    }

    /// Vertices currently held in the chart buffer.
    pub fn chart_vertex_count(&self) -> u32 {
        self.charts.count
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
            if out.len() as u64 + 12 > GRID_CAPACITY {
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

        // The grid is rebuilt from the view every frame, so the view centre
        // is exactly its reference point and its uniform origin is zero.
        self.grid.origin = centre;
        self.grid.count = out.len() as u32;
        self.scratch = out;
    }
}

impl Layer {
    fn new(
        device: &wgpu::Device,
        bind_layout: &wgpu::BindGroupLayout,
        name: &str,
        capacity: u64,
    ) -> Self {
        let vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(name),
            size: capacity.max(1) * std::mem::size_of::<Vertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(name),
            size: std::mem::size_of::<ViewUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(name),
            layout: bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });

        Self {
            vertices,
            capacity: capacity.max(1),
            count: 0,
            origin: chart::WorldXY { x: 0.0, y: 0.0 },
            uniform,
            bind_group,
        }
    }
}

/// Mean of every vertex, used as a layer's reference point.
///
/// The mean rather than the bounding-box centre: it is cheaper, it needs no
/// special case for empty input, and the only property that matters is being
/// *near* the data so the f32 offsets stay small.
fn centroid(lines: &[&[chart::WorldXY]], points: &[chart::WorldXY]) -> chart::WorldXY {
    let mut sum = (0.0f64, 0.0f64);
    let mut n = 0u64;

    for p in lines.iter().flat_map(|l| l.iter()).chain(points.iter()) {
        sum.0 += p.x;
        sum.1 += p.y;
        n += 1;
    }

    if n == 0 {
        return chart::WorldXY { x: 0.0, y: 0.0 };
    }
    chart::WorldXY {
        x: sum.0 / n as f64,
        y: sum.1 / n as f64,
    }
}

/// Half-size of a sounding cross, in world units.
///
/// Derived from the data's own extent so the marks are proportionate to what
/// is loaded: a harbour's worth of soundings gets small crosses, a whole
/// coastline's gets larger ones. A fixed world-unit size would be invisible
/// at one zoom and cover the screen at another, and this buffer cannot know
/// the zoom — it is uploaded once and reused across many.
fn point_arm(lines: &[&[chart::WorldXY]], points: &[chart::WorldXY]) -> f32 {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);

    for p in lines.iter().flat_map(|l| l.iter()).chain(points.iter()) {
        min.0 = min.0.min(p.x);
        min.1 = min.1.min(p.y);
        max.0 = max.0.max(p.x);
        max.1 = max.1.max(p.y);
    }

    let span = (max.0 - min.0).max(max.1 - min.1);
    if span.is_finite() && span > 0.0 {
        (span / 200.0) as f32
    } else {
        // A single point, or none: fall back to something that is visible at
        // a harbour zoom rather than to zero.
        1.0e-6
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rutter_core::chart::WorldXY;

    // Only the pure geometry helpers are tested here. Everything else in
    // this crate needs a GPU and a surface, which is the reason the
    // projection and tile maths live in rutter-core instead.

    fn w(x: f64, y: f64) -> WorldXY {
        WorldXY { x, y }
    }

    #[test]
    fn centroid_of_nothing_is_the_origin() {
        let c = centroid(&[], &[]);
        assert_eq!((c.x, c.y), (0.0, 0.0));
    }

    #[test]
    fn centroid_averages_every_vertex() {
        let line = [w(0.0, 0.0), w(1.0, 1.0)];
        let c = centroid(&[&line], &[w(2.0, 2.0)]);
        assert!((c.x - 1.0).abs() < 1e-12, "x was {}", c.x);
        assert!((c.y - 1.0).abs() < 1e-12, "y was {}", c.y);
    }

    #[test]
    fn centroid_counts_lines_and_points_alike() {
        // Two points on one side, one on the other: the mean must lean.
        let line = [w(0.0, 0.0), w(0.0, 0.0)];
        let c = centroid(&[&line], &[w(0.3, 0.0)]);
        assert!((c.x - 0.1).abs() < 1e-12, "x was {}", c.x);
    }

    #[test]
    fn centroid_stays_near_the_data_far_from_the_origin() {
        // The whole point of a reference point: offsets from it must be
        // small even when the data sits in a corner of the world where f32
        // has no precision left.
        let line = [w(0.999_999_0, 0.999_999_0), w(0.999_999_5, 0.999_999_5)];
        let c = centroid(&[&line], &[]);

        for p in line {
            let offset = ((p.x - c.x) as f32, (p.y - c.y) as f32);
            assert!(offset.0.abs() < 1e-5, "offset {offset:?} is not small");
            // And the offset must survive f32 without collapsing to zero,
            // which is what happens if you store absolute coordinates.
            assert_ne!(offset.0, 0.0);
        }
    }

    #[test]
    fn point_arm_scales_with_the_data_extent() {
        let small = [w(0.5, 0.5), w(0.501, 0.501)];
        let large = [w(0.0, 0.0), w(1.0, 1.0)];
        assert!(
            point_arm(&[&small], &[]) < point_arm(&[&large], &[]),
            "a tighter chart should get smaller marks"
        );
    }

    #[test]
    fn point_arm_is_positive_for_degenerate_input() {
        // A single point has no extent; the mark must still be drawable.
        assert!(point_arm(&[], &[w(0.5, 0.5)]) > 0.0);
        assert!(point_arm(&[], &[]) > 0.0);
        // A vertical line has zero width but real height.
        let vertical = [w(0.5, 0.4), w(0.5, 0.6)];
        assert!(point_arm(&[&vertical], &[]) > 0.0);
    }

    #[test]
    fn point_arm_is_finite() {
        let line = [w(0.0, 0.0), w(1.0, 1.0)];
        assert!(point_arm(&[&line], &[w(0.5, 0.5)]).is_finite());
    }
}
