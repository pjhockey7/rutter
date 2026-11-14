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

pub mod ui;

pub use ui::{
    Action, AlarmState, CurrentState, DisplayState, DownloadProgress, DownloadState, DraftState,
    ImageryState, LayerEntry, PanelState, RouteState, Tab, TemperatureState, TideMarker,
    TideReading, TideState, TimeState, TrackState, Ui, VesselState, WaypointRow, WindState,
};

use rutter_core::chart::{self, ChartStyle, StyledArea, StyledLine, StyledPoint, TileId};
use rutter_core::depth::{self, Clearance, Draft, Tally, TideWindow};
use rutter_core::View;

/// Sea. Everything is drawn over this.
/// How large the boat is drawn by default.
///
/// Above one: the hull was laid out against a desktop window and is a
/// little small even there — it is the one mark on the chart that must
/// never be hunted for.
const DEFAULT_BOAT_SCALE: f32 = 1.25;

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

/// Colour for a chart feature.
///
/// Loosely after the IHO's day palette, which is the convention every
/// chartplotter follows: land buff, shallow water pale, deep water dark,
/// dangers conspicuous. Not a faithful implementation of S-52 — that
/// specifies exact colours per depth band and viewing condition — but close
/// enough that the chart reads the way a navigator expects.
fn colour_for(style: ChartStyle) -> [f32; 4] {
    match style {
        // Land: the buff every paper chart uses.
        ChartStyle::Land => [0.85, 0.78, 0.60, 1.0],
        // Coastline: the strongest line on the chart, so the brightest.
        ChartStyle::Coastline => [0.96, 0.93, 0.85, 1.0],
        // Depth areas and contours: cool and recessive, they are context.
        ChartStyle::DepthArea => [0.35, 0.58, 0.72, 1.0],
        ChartStyle::DepthContour => [0.45, 0.68, 0.82, 1.0],
        // Soundings: legible but quiet; there are hundreds of them.
        ChartStyle::Sounding => [0.62, 0.80, 0.88, 1.0],
        // Marks: warm, so they separate from the blue water behind.
        ChartStyle::Buoy => [0.95, 0.75, 0.20, 1.0],
        ChartStyle::Beacon => [0.90, 0.60, 0.25, 1.0],
        ChartStyle::Light => [0.98, 0.90, 0.35, 1.0],
        // Dangers: the one thing on a chart that should be alarming.
        ChartStyle::Danger => [0.95, 0.35, 0.35, 1.0],
        // Advisory areas: dim magenta, as on paper.
        ChartStyle::Area => [0.70, 0.45, 0.70, 1.0],
        ChartStyle::Other => [0.55, 0.62, 0.68, 1.0],
    }
}

/// Colour for a clearance verdict, when a draft check is switched on.
///
/// These deliberately override the chart palette rather than blending with
/// it. A sounding that the boat cannot pass should not look like a sounding
/// drawn slightly differently; it should look like a warning, and it should
/// be the loudest thing on that part of the chart.
pub(crate) fn colour_for_clearance(clearance: Clearance) -> [f32; 4] {
    match clearance {
        // Green rather than the usual sounding blue: with the check on, the
        // navigator is asking a yes/no question and wants the yes marked.
        Clearance::Clear => [0.35, 0.85, 0.45, 1.0],
        // Amber for "fine now, not later" — the state the chart alone
        // cannot show, and the reason this feature exists.
        Clearance::TightAtLow => [1.0, 0.72, 0.15, 1.0],
        // The same red as a charted danger, because that is what it is.
        Clearance::Foul => [1.0, 0.25, 0.25, 1.0],
    }
}

/// Fill colour for an area, by what it is and how deep.
///
/// Returns `None` for a class that should not be flooded with colour.
///
/// Alpha matters as much as hue here. A chart fill has to tint without
/// obscuring: the aerial imagery sits underneath, and burying it would undo
/// the imagery layer. These are deliberately light.
fn fill_colour_for(style: ChartStyle, value: Option<f64>) -> Option<[f32; 4]> {
    match style {
        // Land: the buff of a paper chart, strong enough to read as solid
        // ground because nothing useful is underneath it.
        ChartStyle::Land => Some([0.78, 0.71, 0.53, 0.85]),
        ChartStyle::DepthArea => {
            // Banded like a paper chart: the shallower the water, the paler
            // and more conspicuous the tint. The bands are S-57's own
            // common ones — 2, 5 and 10 m — rather than invented.
            let depth = value.unwrap_or(f64::MAX);
            Some(if depth < 0.0 {
                // Drying: green on every chart in the world.
                [0.55, 0.62, 0.40, 0.55]
            } else if depth < 2.0 {
                [0.62, 0.78, 0.86, 0.55]
            } else if depth < 5.0 {
                [0.44, 0.63, 0.76, 0.45]
            } else if depth < 10.0 {
                [0.28, 0.47, 0.63, 0.35]
            } else {
                // Deep water is the background, so barely tinted at all.
                [0.14, 0.30, 0.45, 0.25]
            })
        }
        _ => None,
    }
}

/// One cell of a scalar field, as a quad with a colour at each corner.
///
/// The unit of the temperature wash. A quad rather than a triangle because
/// that is the shape a lattice cell is, and per-corner colours rather than
/// one because the GPU's interpolation across the cell is exactly the
/// bilinear sampling the underlying grid already uses — so the wash is
/// smooth for free, and refining the lattice costs nothing but vertices.
///
/// Corners in order: north-west, north-east, south-east, south-west.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldPatch {
    pub corners: [chart::WorldXY; 4],
    pub colours: [[f32; 4]; 4],
}

/// The temperature scale, in degrees Celsius and the colour for each.
///
/// Stops rather than a formula, so the scale is legible and adjustable
/// without algebra, and so the panel's legend can be built from the same
/// list the wash is. Interpolated between; clamped at the ends.
///
/// Chosen for air over coastal water, which is a much narrower band than
/// air over land: blue below ten, green through the teens, yellow and red
/// above twenty-five. Somewhere in the twenties should read as pleasant
/// rather than as a warning, which rules out the usual meteorological
/// red-for-hot scale starting too low.
pub const TEMPERATURE_STOPS: [(f32, [f32; 3]); 7] = [
    (-5.0, [0.48, 0.35, 0.78]),
    (0.0, [0.27, 0.45, 0.85]),
    (8.0, [0.25, 0.72, 0.85]),
    (15.0, [0.35, 0.78, 0.45]),
    (21.0, [0.92, 0.86, 0.35]),
    (27.0, [0.95, 0.56, 0.22]),
    (34.0, [0.88, 0.25, 0.25]),
];

/// The colour for a temperature, at a given opacity.
///
/// Public because the panel's legend has to use the same scale as the
/// wash: a legend that disagreed with the chart would be worse than none.
pub fn temperature_colour(celsius: f32, opacity: f32) -> [f32; 4] {
    let alpha = opacity.clamp(0.0, 1.0);
    // NaN reads as the coldest stop rather than propagating into a colour
    // channel, where it would come out as a hole in the wash. A GRIB file
    // can carry a missing value, and a hole that looked like land would be
    // the wrong way to show it.
    if celsius.is_nan() {
        let [r, g, b] = TEMPERATURE_STOPS[0].1;
        return [r, g, b, alpha];
    }

    let first = TEMPERATURE_STOPS[0];
    let last = TEMPERATURE_STOPS[TEMPERATURE_STOPS.len() - 1];
    if celsius <= first.0 {
        return [first.1[0], first.1[1], first.1[2], alpha];
    }
    if celsius >= last.0 {
        return [last.1[0], last.1[1], last.1[2], alpha];
    }

    for pair in TEMPERATURE_STOPS.windows(2) {
        let (low, cold) = pair[0];
        let (high, warm) = pair[1];
        if celsius <= high {
            // `high > low` holds for every pair in the table, so this
            // cannot divide by zero — and a table edited into a tie would
            // be caught by `the_temperature_scale_is_ordered`.
            let t = (celsius - low) / (high - low);
            return [
                cold[0] + (warm[0] - cold[0]) * t,
                cold[1] + (warm[1] - cold[1]) * t,
                cold[2] + (warm[2] - cold[2]) * t,
                alpha,
            ];
        }
    }
    [last.1[0], last.1[1], last.1[2], alpha]
}

/// Whether a style's value really is a depth below chart datum.
///
/// A light's HEIGHT is a height *above* datum and a buoy has no depth at
/// all, so running either through the under-keel arithmetic would paint
/// confident nonsense. This is the gate that stops it.
fn judgeable(style: ChartStyle) -> bool {
    matches!(
        style,
        ChartStyle::Sounding
            | ChartStyle::Danger
            | ChartStyle::DepthArea
            | ChartStyle::DepthContour
    )
}

/// Fill colour for a clearance verdict.
///
/// `Clear` returns `None` on purpose. With the check on, the answer wanted
/// is "where can I not go", and shading the safe water as well would bury
/// that in green. Leaving it unshaded keeps the chart and the imagery
/// readable everywhere the question does not apply.
///
/// The shader does the shading now, so nothing calls this at runtime — but
/// it is still the *specification* of that palette, and
/// `the_shader_palette_matches_the_rust_one` reads the constants out of
/// `chart.wgsl` and checks them against it. Deleting this would leave the
/// shader's colours unchecked.
#[cfg(test)]
fn clearance_fill_for(clearance: Clearance) -> Option<[f32; 4]> {
    match clearance {
        Clearance::Clear => None,
        Clearance::TightAtLow => Some([1.0, 0.72, 0.15, 0.40]),
        Clearance::Foul => Some([1.0, 0.25, 0.25, 0.45]),
    }
}

/// Everything needed to judge a charted depth: the boat and the tide.
///
/// Held as one value so that switching the check on and off is one
/// assignment, and so that a stale tide cannot be paired with a fresh
/// draft.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DraftCheck {
    pub draft: Draft,
    pub tide: TideWindow,
}

/// Half-size of a point mark, as a multiple of the computed base arm.
///
/// Soundings are the smallest because they are the most numerous; a danger
/// is drawn large because missing one matters more than tidiness.
fn mark_scale(style: ChartStyle) -> f32 {
    match style {
        ChartStyle::Sounding => 0.7,
        ChartStyle::Buoy | ChartStyle::Beacon => 1.3,
        ChartStyle::Light => 1.6,
        ChartStyle::Danger => 1.8,
        _ => 1.0,
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    /// Offset from this buffer's reference point, in normalised world units.
    offset: [f32; 2],
    /// A further offset, scaled on the GPU by how emphatic the feature's
    /// clearance verdict is. Zero for everything except the arms of a
    /// sounding's cross, which is the one mark whose *size* carries
    /// meaning and so could not simply be recoloured when the tide moved.
    arm: [f32; 2],
    /// The colour to use when the under-keel check does not apply — the
    /// style's own colour, or a depth area's band.
    colour: [f32; 4],
    /// Charted depth below datum, metres, or [`NO_DEPTH`] for a feature
    /// the check must not judge. See `chart.wgsl`.
    depth: f32,
}

/// What a vertex carries in `depth` when it is not a depth.
///
/// Must match `NO_DEPTH` in `chart.wgsl`; the shader treats anything at or
/// below it as unjudgeable. A sentinel rather than NaN because WGSL makes
/// no promises about NaN comparisons.
const NO_DEPTH: f32 = -1.0e8;

impl Vertex {
    /// A vertex with no clearance meaning: the tile grid, a coastline, a
    /// light's height, a buoy.
    fn plain(offset: [f32; 2], colour: [f32; 4]) -> Self {
        Self {
            offset,
            arm: [0.0, 0.0],
            colour,
            depth: NO_DEPTH,
        }
    }

    /// A vertex the under-keel check may judge, carrying its charted depth.
    fn judged(offset: [f32; 2], colour: [f32; 4], depth: Option<f64>) -> Self {
        Self {
            offset,
            arm: [0.0, 0.0],
            colour,
            depth: depth.map_or(NO_DEPTH, |d| d as f32),
        }
    }

    fn with_arm(mut self, arm: [f32; 2]) -> Self {
        self.arm = arm;
        self
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageryVertex {
    /// Offset from the imagery layer's reference point, in world units.
    offset: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageryUniform {
    scale: [f32; 2],
    origin: [f32; 2],
    opacity: f32,
    /// WGSL rounds a struct up to a 16-byte boundary; spelling the padding
    /// out keeps the Rust and WGSL layouts obviously identical rather than
    /// coincidentally so.
    _pad: [f32; 3],
}

/// One imagery tile on the GPU: its texture, its binding, and where its
/// quad sits in the vertex buffer.
struct ImageryTile {
    id: TileId,
    bind_group: wgpu::BindGroup,
    /// Index of this tile's first vertex. Six per tile — two triangles,
    /// unindexed, because at a hundred-odd tiles the index buffer would
    /// save a few kilobytes and cost a chunk of bookkeeping.
    first_vertex: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OverlayVertex {
    /// Offset from the overlay buffer's reference point, world units.
    anchor: [f32; 2],
    /// Offset from the anchor, screen pixels.
    pixel: [f32; 2],
    colour: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OverlayUniform {
    scale: [f32; 2],
    origin: [f32; 2],
    viewport: [f32; 2],
    /// See the note in overlay.wgsl: 24 bytes rounded to 32.
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    scale: [f32; 2],
    /// This buffer's reference point relative to the view centre.
    origin: [f32; 2],
    /// The under-keel check: `[on, needed_m, tide_now_m, tide_lowest_m]`.
    ///
    /// Four floats rather than four fields so the struct is one 32-byte
    /// block with no padding to get wrong. See `chart.wgsl`.
    check: [f32; 4],
}

impl ViewUniform {
    /// The check, as the shader wants it.
    fn check_of(draft_check: Option<DraftCheck>) -> [f32; 4] {
        match draft_check {
            Some(check) => [
                1.0,
                check.draft.needed_m() as f32,
                check.tide.now_m as f32,
                check.tide.lowest_m as f32,
            ],
            // Nothing is judged, so the other three are never read.
            None => [0.0, 0.0, 0.0, 0.0],
        }
    }
}

/// A mark anchored to a chart position but drawn at a fixed screen size.
///
/// Chart notation rather than chart geometry: a station or a barb marks a
/// place without claiming an extent, so it should not grow with the zoom.
/// See `overlay.wgsl` for how that is done.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Overlay {
    /// A circle, for a station or for calm air.
    Ring {
        at: chart::WorldXY,
        radius_px: f32,
        /// A second, inner ring. Used to pick out the selected station
        /// without needing a different colour.
        emphasised: bool,
        colour: [f32; 4],
    },
    /// Where the boat is, from a GPS fix.
    ///
    /// Drawn as a pointed hull when the course is known and as a ringed
    /// cross when it is not — the same distinction a paper chart draws
    /// between a fix with a course and a dead-reckoning position.
    Boat {
        at: chart::WorldXY,
        /// Which way the bow points, degrees true. Course over ground
        /// where the boat is moving, heading where it is not. `None` when
        /// neither is known: a position alone is still worth marking, and
        /// a symbol pointing north by default is a lie about the heading.
        course_deg: Option<f32>,
        colour: [f32; 4],
    },
    /// One leg of a route: a line between two places on the chart.
    ///
    /// The only mark here drawn in *world* space at both ends rather than
    /// anchored at one and sized in pixels. A leg has a real extent — it
    /// is sixty miles of water — so it has to grow and shrink with the
    /// zoom like a coastline, not stay a fixed length on screen like a
    /// symbol.
    Leg {
        from: chart::WorldXY,
        to: chart::WorldXY,
        colour: [f32; 4],
    },
    /// A waypoint: a place you are steering for.
    Waypoint {
        at: chart::WorldXY,
        /// The one being steered for now, drawn with a ring around it.
        active: bool,
        colour: [f32; 4],
    },
    /// A tidal current: an arrow the way the water is going.
    ///
    /// Length carries the speed, which is the opposite choice from a wind
    /// barb's feathers — and deliberate. A current is read as "which way
    /// and roughly how hard", often at a glance while looking for the
    /// channel, and a long arrow says that faster than a count of
    /// feathers. The exact number is in the panel.
    Current {
        at: chart::WorldXY,
        speed_kn: f32,
        /// Where the water is *going*, degrees true. The opposite
        /// convention to a wind, which is named for where it comes from —
        /// and the convention every chart and tidal atlas uses for a
        /// current.
        toward_deg: f32,
        colour: [f32; 4],
    },
    /// A meteorological wind barb: half 5 kn, full 10, pennant 50.
    Barb {
        at: chart::WorldXY,
        speed_kn: f32,
        /// Where the wind comes *from*, degrees true — which is how a wind
        /// is named, and the way the shaft points.
        from_deg: f32,
        colour: [f32; 4],
    },
}

impl Overlay {
    fn at(&self) -> chart::WorldXY {
        match self {
            Self::Ring { at, .. }
            | Self::Barb { at, .. }
            | Self::Boat { at, .. }
            | Self::Waypoint { at, .. }
            | Self::Current { at, .. } => *at,
            // A leg's reference point is its start; the far end carries
            // its own world anchor. See the `Leg` arm in `set_overlay`.
            Self::Leg { from, .. } => *from,
        }
    }
}

/// How a wind speed decomposes into barb feathers.
///
/// The convention: a pennant is 50 knots, a full barb 10, a half barb 5,
/// and the speed is rounded to the nearest 5 before being split. Pure
/// arithmetic, separated from the drawing so it can be checked — a barb
/// showing 25 knots as 35 is a readable, confident, wrong symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Feathers {
    pennants: u8,
    full: u8,
    half: bool,
}

/// Below this there is no direction worth drawing, and the symbol is a bare
/// circle rather than a shaft.
const CALM_KN: f32 = 2.5;

/// Below this a current is slack and has no direction worth drawing.
///
/// Much lower than [`CALM_KN`], because a tenth of a knot of wind is
/// nothing and a tenth of a knot of current is still half a mile in five
/// hours. Slack water is genuinely close to zero.
const SLACK_KN: f32 = 0.1;

/// Arrow length in pixels for a current, slack to strong.
///
/// Clamped at both ends: a 4-knot race and a 6-knot one are both "more
/// than you can motor against", and an arrow that kept growing would run
/// across the chart saying nothing extra.
const CURRENT_SHORTEST_PX: f32 = 12.0;
const CURRENT_LONGEST_PX: f32 = 38.0;
/// The speed at which an arrow reaches its full length.
const CURRENT_FULL_SCALE_KN: f32 = 3.0;

/// How long to draw a current arrow.
fn current_length_px(speed_kn: f32) -> f32 {
    // `partial_cmp` rather than a comparison, so a NaN from a malformed
    // station reads as slack instead of as an arrow off the screen.
    let fraction = (speed_kn / CURRENT_FULL_SCALE_KN).clamp(0.0, 1.0);
    if fraction.is_nan() {
        return CURRENT_SHORTEST_PX;
    }
    CURRENT_SHORTEST_PX + (CURRENT_LONGEST_PX - CURRENT_SHORTEST_PX) * fraction
}

fn feathers(speed_kn: f32) -> Feathers {
    // Rounded to the nearest five, which is the resolution the symbol has.
    //
    // Spelled with `is_none_or` rather than `!(speed >= CALM)` so that the
    // NaN case is deliberate rather than a side effect of negating a
    // partial comparison: a grid can carry NaN where a model has no data,
    // and that must read as calm rather than as a hurricane.
    if speed_kn
        .partial_cmp(&CALM_KN)
        .is_none_or(|o| o == core::cmp::Ordering::Less)
    {
        return Feathers {
            pennants: 0,
            full: 0,
            half: false,
        };
    }
    let mut fives = (speed_kn / 5.0).round().max(0.0) as u32;
    let pennants = (fives / 10).min(u8::MAX as u32) as u8;
    fives -= u32::from(pennants) * 10;
    let full = (fives / 2) as u8;
    Feathers {
        pennants,
        full,
        half: fives % 2 == 1,
    }
}

/// Unit vector along a barb's shaft, in pixels.
///
/// A bearing is clockwise from north; pixel y grows downward. Getting this
/// wrong mirrors every barb about the horizontal, which looks very nearly
/// right.
fn shaft(from_deg: f32) -> (f32, f32) {
    let radians = from_deg.to_radians();
    (radians.sin(), -radians.cos())
}

/// How strongly imagery shows through by default.
///
/// Not fully opaque: chart linework has to stay readable over an aerial
/// photograph, and a slight fade buys that contrast without restyling every
/// line. Adjustable at runtime — on a clear day you may want the photograph.
const DEFAULT_IMAGERY_OPACITY: f32 = 0.75;

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

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    /// Same vertices and shader as `pipeline`, triangles instead of lines.
    fill_pipeline: wgpu::RenderPipeline,
    /// Chart notation: anchored in the world, sized in pixels.
    overlay_pipeline: wgpu::RenderPipeline,
    overlay_bind: wgpu::BindGroup,
    overlay_uniform: wgpu::Buffer,
    overlay_vertices: wgpu::Buffer,
    overlay_capacity: u64,
    overlay_count: u32,
    /// Solid overlay geometry — the boat's hull. Its own buffer because it
    /// is drawn by a different pipeline, and tiny because only one mark
    /// uses it.
    overlay_fill_pipeline: wgpu::RenderPipeline,
    overlay_fill_vertices: wgpu::Buffer,
    overlay_fill_capacity: u64,
    overlay_fill_count: u32,
    /// Reference point the overlay anchors are measured from.
    overlay_origin: chart::WorldXY,
    /// The part of the surface the chart may use, in surface pixels, or
    /// `None` for all of it. See [`Renderer::set_letterbox`].
    letterbox: Option<(u32, u32, u32, u32)>,
    /// How large to draw the boat. See [`Renderer::set_boat_scale`].
    boat_scale: f32,
    bind_layout: wgpu::BindGroupLayout,
    /// egui's wgpu backend, which owns its own pipelines and font atlas and
    /// draws into the chart's render pass.
    ///
    /// Not `Debug`, hence the manual impl for `Renderer` below.
    egui: egui_wgpu::Renderer,
    /// Tile boundaries and the centre mark. Rebuilt every frame.
    grid: Layer,
    /// Real chart geometry. Uploaded on change, then left alone.
    charts: Layer,
    /// Filled areas, under the linework. Its own buffer because it is
    /// drawn by a different pipeline and sized quite differently: a tile's
    /// fills are thousands of triangles where its outlines are hundreds of
    /// segments.
    fills: Layer,
    /// The temperature wash: a coloured lattice over the water.
    ///
    /// Its own layer rather than part of the chart's fills because it is a
    /// different kind of thing with a different lifetime — the chart
    /// changes when the tiles do, and this changes when the clock or the
    /// time slider does.
    temperature: Layer,

    /// Aerial imagery, drawn under everything else.
    imagery_pipeline: wgpu::RenderPipeline,
    imagery_texture_layout: wgpu::BindGroupLayout,
    imagery_uniform: wgpu::Buffer,
    imagery_view_bind: wgpu::BindGroup,
    imagery_vertices: wgpu::Buffer,
    imagery_capacity: u64,
    imagery_tiles: Vec<ImageryTile>,
    /// Reference point the imagery quads are measured from, in f64 so the
    /// subtraction against the view centre keeps its precision.
    imagery_origin: chart::WorldXY,
    /// How strongly the imagery shows through, 0..1.
    imagery_opacity: f32,
    /// Shared by every imagery tile: linear filtering, clamped at the edges
    /// so a tile does not bleed its neighbour's pixels across the seam.
    imagery_sampler: wgpu::Sampler,
    /// Scratch, kept between frames so a redraw does not allocate.
    scratch: Vec<Vertex>,

    /// The boat and the tide, when the navigator has asked the question.
    /// `None` means draw the chart in its own colours.
    draft_check: Option<DraftCheck>,
    /// What the last chart upload judged, for the panel to report.
    /// Charted depths of the features in view that the under-keel check
    /// may judge, kept so the panel's tally can be recomputed when the
    /// tide moves without touching the vertex buffers.
    judged: Vec<f64>,
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

impl std::fmt::Debug for Renderer {
    /// Written by hand because `egui_wgpu::Renderer` is not `Debug`, and the
    /// workspace lints ask every public type to be. The useful fields are
    /// the surface size and what is currently uploaded; the GPU handles
    /// would print as opaque ids.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("size", &(self.config.width, self.config.height))
            .field("format", &self.config.format)
            .field("grid_vertices", &self.grid.count)
            .field("chart_vertices", &self.charts.count)
            .finish_non_exhaustive()
    }
}

/// One frame of interface, ready to composite.
///
/// The primitive list is borrowed, because it is the tessellated interface
/// and can be large. The texture delta is **owned**, because it must be: it
/// carries a `Drop` assertion that fires if it is dropped with deltas still
/// unapplied, so a painter has to consume it rather than read it. Taking it
/// by value here makes that the type's problem instead of the caller's.
#[derive(Debug)]
pub struct GuiFrame<'a> {
    pub primitives: &'a [egui::ClippedPrimitive],
    pub textures: egui::TexturesDelta,
    pub pixels_per_point: f32,
}

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
            // Fifo is vsync: present blocks until the next refresh, which
            // is what a chart on a battery wants.
            //
            // RUTTER_NO_VSYNC exists for measurement. With Fifo every frame
            // reads as 16.6 ms whether the GPU is idle or nearly drowning,
            // so it is impossible to tell a cheap frame from an expensive
            // one — which is exactly the question when judging whether a
            // new layer costs too much.
            present_mode: if std::env::var_os("RUTTER_NO_VSYNC").is_some() {
                wgpu::PresentMode::AutoNoVsync
            } else {
                wgpu::PresentMode::Fifo
            },
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
                        1 => Float32x2,
                        2 => Float32x4,
                        3 => Float32,
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

        // Filled areas: the same vertices, the same shader, the same bind
        // group — only the topology differs. A second pipeline rather than
        // a second shader, because a triangle and a line want exactly the
        // same "offset from a reference point, plus a colour" treatment.
        //
        // No culling. Ear clipping gives consistent winding, but relying on
        // that would mean a whole depth area silently vanishing if a ring's
        // orientation were ever misread, and a chart that loses its shallow
        // water without saying so is the wrong failure.
        let fill_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("chart fills"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_fill"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x4,
                        3 => Float32,
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
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let fills = Layer::new(&device, &bind_layout, "fills", GRID_CAPACITY);
        let temperature = Layer::new(&device, &bind_layout, "temperature", GRID_CAPACITY);

        // ── Overlay pipeline ───────────────────────────────────────────
        //
        // Its own pipeline because the vertex layout differs: two offsets
        // rather than one, and a uniform that carries the viewport so the
        // shader can turn pixels into clip units. Sharing the chart's
        // pipeline would mean a vertex format with an unused field on every
        // coastline vertex, which is the larger buffer by far.
        let overlay_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay"),
            source: wgpu::ShaderSource::Wgsl(include_str!("overlay.wgsl").into()),
        });

        let overlay_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay view"),
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

        let overlay_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay view"),
            size: std::mem::size_of::<OverlayUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let overlay_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("overlay view"),
            layout: &overlay_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: overlay_uniform.as_entire_binding(),
            }],
        });

        let overlay_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("overlay"),
                bind_group_layouts: &[Some(&overlay_layout)],
                immediate_size: 0,
            });

        // The same shader, the same vertices, triangles instead of lines.
        // Only the boat uses it: a station ring or a wind barb is notation
        // and should stay as thin as a pencil line, while the boat is the
        // one mark that must be found instantly on a busy chart — and a
        // hollow outline a few pixels wide is exactly what gets lost
        // against a sounding.
        let overlay_fill_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("overlay fills"),
                layout: Some(&overlay_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &overlay_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<OverlayVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![
                            0 => Float32x2,
                            1 => Float32x2,
                            2 => Float32x4,
                        ],
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &overlay_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    // No culling: a mark's triangles are wound however the
                    // arithmetic happened to produce them, and a boat that
                    // vanished when it turned south would be a memorable bug.
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        let overlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay"),
            layout: Some(&overlay_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &overlay_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<OverlayVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x4,
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &overlay_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // 256 vertices is far more than the handful a hull needs, and
        // costs four kilobytes.
        let overlay_fill_vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay fills"),
            size: std::mem::size_of::<OverlayVertex>() as u64 * 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let overlay_vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay"),
            size: std::mem::size_of::<OverlayVertex>() as u64 * 4096,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // ── Imagery pipeline ───────────────────────────────────────────
        //
        // A second pipeline rather than a branch in the first: textured
        // quads and coloured lines need different vertex layouts, different
        // topologies and different bind groups, and merging them would mean
        // a shader that does neither job clearly.
        let imagery_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("imagery"),
            source: wgpu::ShaderSource::Wgsl(include_str!("imagery.wgsl").into()),
        });

        let imagery_view_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("imagery view"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let imagery_texture_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("imagery tile"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let imagery_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("imagery"),
            bind_group_layouts: &[Some(&imagery_view_layout), Some(&imagery_texture_layout)],
            immediate_size: 0,
        });

        let imagery_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("imagery"),
            layout: Some(&imagery_layout),
            vertex: wgpu::VertexState {
                module: &imagery_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImageryVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &imagery_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let imagery_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("imagery view"),
            size: std::mem::size_of::<ImageryUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let imagery_view_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("imagery view"),
            layout: &imagery_view_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: imagery_uniform.as_entire_binding(),
            }],
        });

        let imagery_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("imagery"),
            // Clamp, not repeat: at a tile's edge, repeating wraps the
            // opposite side of the photograph into the seam.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let imagery_vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("imagery"),
            size: std::mem::size_of::<ImageryVertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let grid = Layer::new(&device, &bind_layout, "grid", GRID_CAPACITY);
        // The chart layer starts empty; `set_chart` sizes it to the data.
        let charts = Layer::new(&device, &bind_layout, "charts", 1);

        // No depth buffer and no MSAA, matching the chart pipeline.
        // Dithering on, which is egui's own default and matters for the flat
        // panel fills this draws.
        let egui = egui_wgpu::Renderer::new(
            &device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: true,
                predictable_texture_filtering: false,
            },
        );

        Ok(Self {
            // A window fills itself; only a phone has anything to avoid.
            letterbox: None,
            boat_scale: DEFAULT_BOAT_SCALE,
            surface,
            device,
            queue,
            config,
            pipeline,
            fill_pipeline,
            overlay_pipeline,
            overlay_bind,
            overlay_uniform,
            overlay_vertices,
            overlay_capacity: 4096,
            overlay_count: 0,
            overlay_fill_pipeline,
            overlay_fill_vertices,
            overlay_fill_capacity: 256,
            overlay_fill_count: 0,
            overlay_origin: chart::WorldXY { x: 0.0, y: 0.0 },
            bind_layout,
            egui,
            grid,
            charts,
            fills,
            temperature,
            imagery_pipeline,
            imagery_texture_layout,
            imagery_uniform,
            imagery_view_bind,
            imagery_vertices,
            imagery_capacity: 1,
            imagery_tiles: Vec::new(),
            imagery_origin: chart::WorldXY { x: 0.0, y: 0.0 },
            imagery_opacity: DEFAULT_IMAGERY_OPACITY,
            imagery_sampler,
            scratch: Vec::with_capacity(4096),
            // Off until the navigator supplies a draft. Defaulting it on
            // with a guessed draft would paint confident red over a chart
            // nobody asked a question about.
            draft_check: None,
            judged: Vec::new(),
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
    /// How large to draw the boat, as a multiple of the base size.
    ///
    /// A platform's business, because the thing it has to correct for is
    /// a platform fact: the hull is laid out in *physical* pixels, and a
    /// phone has two or three of those per point. Left alone, the one
    /// mark that must never be hunted for came out a third of the size
    /// on the device where the screen is smallest.
    ///
    /// Clamped rather than trusted — a zero would make the boat vanish
    /// and a huge value would cover the chart, and neither should be
    /// reachable by a platform getting its arithmetic wrong.
    pub fn set_boat_scale(&mut self, scale: f32) {
        self.boat_scale = if scale.is_finite() {
            scale.clamp(0.5, 6.0)
        } else {
            DEFAULT_BOAT_SCALE
        };
    }

    /// Confine the chart to part of the surface.
    ///
    /// `None` fills it, which is what a desktop window wants. A phone
    /// passes the area the system has *not* claimed — see
    /// `platform/android/src/insets.rs` — so the chart stops short of a
    /// camera cutout and a gesture bar rather than being drawn under
    /// them.
    ///
    /// Only the chart is confined. The interface is drawn afterwards with
    /// the viewport restored, because it works in surface pixels and
    /// paints the margins itself.
    pub fn set_letterbox(&mut self, area: Option<(u32, u32, u32, u32)>) {
        self.letterbox = area.filter(|&(_, _, w, h)| w > 0 && h > 0);
    }

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

    /// Draw one frame of `view`, with no interface over it.
    pub fn render(&mut self, view: &View) -> Frame {
        self.render_with_ui(view, None)
    }

    /// Draw one frame of `view`, compositing an egui frame over it.
    ///
    /// The interface shares the chart's render pass rather than taking its
    /// own, which is why egui was chosen: one surface, one submit, and the
    /// panel is simply drawn last.
    pub fn render_with_ui(&mut self, view: &View, mut gui: Option<GuiFrame<'_>>) -> Frame {
        use wgpu::CurrentSurfaceTexture as Acquired;

        self.build_geometry(view);
        self.upload(view);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        // egui's textures and buffers are updated here — deliberately before
        // the surface is acquired, and the delta is drained rather than read.
        //
        // The ordering is forced by `TexturesDelta`'s Drop assertion, which
        // fires if it is dropped with anything still unapplied. Acquiring
        // the surface first would mean the early returns below — timed out,
        // occluded, lost — drop the delta unapplied, and a panic inside Drop
        // mid-frame surfaces as a segfault rather than as an error. That is
        // exactly the bug this ordering replaced.
        let mut egui_prepared = None;
        let mut to_free: Vec<egui::TextureId> = Vec::new();
        if let Some(gui) = &mut gui {
            // One texture can have several deltas queued in a frame — a
            // partial font-atlas update followed by another — so this is a
            // map of id to a list, not a single delta each.
            for (id, deltas) in gui.textures.set.drain() {
                for delta in deltas {
                    self.egui
                        .update_texture(&self.device, &self.queue, id, &delta);
                }
            }
            // Freed textures are released after the pass, since the pass may
            // still reference them; the ids are taken now so the delta can
            // be emptied.
            to_free = gui.textures.free.drain().collect();
            debug_assert!(gui.textures.set.is_empty() && gui.textures.free.is_empty());

            let descriptor = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.config.width, self.config.height],
                pixels_per_point: gui.pixels_per_point,
            };
            self.egui.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                gui.primitives,
                &descriptor,
            );
            egui_prepared = Some(descriptor);
        }

        // Set by the Suboptimal arm below, and acted on only after the
        // frame has been presented. Reconfiguring while a surface output is
        // still alive is a validation error in wgpu 30 — "the SurfaceOutput
        // returned by get_current_texture must be dropped before
        // re-configuring" — and since errors are fatal by default it is a
        // panic, not a warning.
        //
        // This crashed on X11 and not on Wayland, which is why it survived:
        // X11 guesses a scale factor and resizes the window immediately
        // after creating it, so the very first acquisition comes back
        // Suboptimal. Wayland sizes the surface before the first frame and
        // never takes this path.
        let mut reconfigure_after_presenting = false;

        let frame = match self.surface.get_current_texture() {
            Acquired::Success(f) => f,
            // Suboptimal still gives a usable texture; draw into it and fix
            // the configuration for next time rather than dropping a frame.
            Acquired::Suboptimal(f) => {
                reconfigure_after_presenting = true;
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

            // Everything from here to the interface is drawn into the
            // part of the surface the system has left us. The clear
            // above still covers the whole attachment, so the margins
            // come out sea-coloured; the panel paints them over.
            if let Some((x, y, w, h)) = self.letterbox {
                pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
            }

            // Imagery first of all: a photograph is the ground everything
            // else is drawn on.
            if !self.imagery_tiles.is_empty() {
                pass.set_pipeline(&self.imagery_pipeline);
                pass.set_bind_group(0, &self.imagery_view_bind, &[]);
                pass.set_vertex_buffer(0, self.imagery_vertices.slice(..));
                for tile in &self.imagery_tiles {
                    pass.set_bind_group(1, &tile.bind_group, &[]);
                    pass.draw(tile.first_vertex..tile.first_vertex + 6, 0..1);
                }
            }

            // The temperature wash above the photograph and below
            // everything else. It is weather over the ground rather than a
            // feature of it, and the chart's own linework has to stay
            // readable through it — which is why it is drawn first of the
            // translucent layers and why its opacity is adjustable.
            if self.temperature.count > 0 {
                pass.set_pipeline(&self.fill_pipeline);
                pass.set_bind_group(0, &self.temperature.bind_group, &[]);
                pass.set_vertex_buffer(0, self.temperature.vertices.slice(..));
                pass.draw(0..self.temperature.count, 0..1);
            }

            // Fills under everything the line pipeline draws, so the
            // linework reads on top of its own area's tint rather than
            // being painted over by it.
            if self.fills.count > 0 {
                pass.set_pipeline(&self.fill_pipeline);
                pass.set_bind_group(0, &self.fills.bind_group, &[]);
                pass.set_vertex_buffer(0, self.fills.vertices.slice(..));
                pass.draw(0..self.fills.count, 0..1);
            }

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

            // Notation over the chart, under the interface: a station ring
            // belongs on top of the water it marks and underneath the panel
            // that explains it.
            // Solid marks first, their outlines over the top: the hull is
            // filled and then edged, which is what makes it read as a
            // shape rather than as a smudge.
            if self.overlay_fill_count > 0 {
                pass.set_pipeline(&self.overlay_fill_pipeline);
                pass.set_bind_group(0, &self.overlay_bind, &[]);
                pass.set_vertex_buffer(0, self.overlay_fill_vertices.slice(..));
                pass.draw(0..self.overlay_fill_count, 0..1);
            }
            if self.overlay_count > 0 {
                pass.set_pipeline(&self.overlay_pipeline);
                pass.set_bind_group(0, &self.overlay_bind, &[]);
                pass.set_vertex_buffer(0, self.overlay_vertices.slice(..));
                pass.draw(0..self.overlay_count, 0..1);
            }

            // Interface last, over everything — and over the whole
            // surface, not the letterbox. egui's vertices and scissor
            // rects are in surface pixels, so a viewport still set to the
            // safe area would shift and squash the entire panel.
            if self.letterbox.is_some() {
                pass.set_viewport(
                    0.0,
                    0.0,
                    self.config.width as f32,
                    self.config.height as f32,
                    0.0,
                    1.0,
                );
            }
            if let (Some(gui), Some(descriptor)) = (&gui, &egui_prepared) {
                self.egui
                    .render(&mut pass.forget_lifetime(), gui.primitives, descriptor);
            }
        }

        // Release textures egui dropped this frame. Skipping this leaks GPU
        // memory slowly, which is the worst kind of leak to find later.
        for id in &to_free {
            self.egui.free_texture(id);
        }

        self.queue.submit(Some(encoder.finish()));
        // Presenting is the queue's job in wgpu 30, not the texture's. It
        // also consumes the surface output, which is what makes it safe to
        // reconfigure immediately afterwards.
        self.queue.present(frame);

        if reconfigure_after_presenting {
            self.reconfigure();
            // The frame was drawn, but at the old configuration, so the
            // caller is asked for another one.
            return Frame::Reconfigured;
        }
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

    /// Refresh every layer's uniform, and the grid's vertices.
    ///
    /// The chart's vertices are deliberately absent: they were uploaded when
    /// the tile set changed and are still correct, because they are measured
    /// from the chart layer's own origin rather than from the view.
    fn upload(&mut self, view: &View) {
        let scale = self.scale_for(view);
        let centre = chart::project(view.centre);

        // The overlay's own uniform, which carries the viewport as well.
        let overlay = OverlayUniform {
            scale,
            origin: [
                (self.overlay_origin.x - centre.x) as f32,
                (self.overlay_origin.y - centre.y) as f32,
            ],
            viewport: [self.config.width as f32, self.config.height as f32],
            _pad: [0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.overlay_uniform, 0, bytemuck::bytes_of(&overlay));

        // The fills share the chart's origin, but they have their own
        // uniform buffer and so need their own write — a fill layer left
        // on a stale origin slides off its own coastline as you pan.
        for layer in [&self.grid, &self.charts, &self.fills, &self.temperature] {
            // The subtraction happens in f64 and only the small difference
            // becomes f32. Projecting to f32 first would throw away the
            // precision this whole arrangement exists to keep.
            let uniform = ViewUniform {
                scale,
                origin: [
                    (layer.origin.x - centre.x) as f32,
                    (layer.origin.y - centre.y) as f32,
                ],
                // Written every frame along with the origin, which is what
                // makes the under-keel check free to change: a new tide is
                // 32 bytes, not a chart.
                check: ViewUniform::check_of(self.draft_check),
            };
            self.queue
                .write_buffer(&layer.uniform, 0, bytemuck::bytes_of(&uniform));
        }

        if self.grid.count > 0 {
            self.queue
                .write_buffer(&self.grid.vertices, 0, bytemuck::cast_slice(&self.scratch));
        }

        // The imagery layer's own uniform. Same reference-point arithmetic,
        // its own buffer because it carries an opacity the line shader has
        // no use for.
        if !self.imagery_tiles.is_empty() {
            let uniform = ImageryUniform {
                scale,
                origin: [
                    (self.imagery_origin.x - centre.x) as f32,
                    (self.imagery_origin.y - centre.y) as f32,
                ],
                opacity: self.imagery_opacity,
                _pad: [0.0; 3],
            };
            self.queue
                .write_buffer(&self.imagery_uniform, 0, bytemuck::bytes_of(&uniform));
        }
    }
}

/// Mean centre of a set of imagery tiles, used as their reference point.
fn imagery_origin(tiles: &[chart::TileImage]) -> chart::WorldXY {
    if tiles.is_empty() {
        return chart::WorldXY { x: 0.0, y: 0.0 };
    }
    let mut sum = (0.0f64, 0.0f64);
    for tile in tiles {
        let n = f64::from(TileId::grid_size(tile.id.z));
        sum.0 += (f64::from(tile.id.x) + 0.5) / n;
        sum.1 += (f64::from(tile.id.y) + 0.5) / n;
    }
    let count = tiles.len() as f64;
    chart::WorldXY {
        x: sum.0 / count,
        y: sum.1 / count,
    }
}

impl Renderer {
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
    /// Switch the draft check on, or off with `None`.
    ///
    /// Does not itself redraw: the caller clears its loaded-tile list so the
    /// geometry is rebuilt, the same way a layer toggle works. Colours live
    /// in the vertex buffer, so changing a verdict means re-uploading it.
    pub fn set_draft_check(&mut self, check: Option<DraftCheck>) {
        self.draft_check = check;
    }

    pub fn draft_check(&self) -> Option<DraftCheck> {
        self.draft_check
    }

    /// How the features in view are judged, for the panel.
    ///
    /// Recomputed on demand rather than stored, because the verdict now
    /// lives in the shader: `set_chart` keeps only the list of charted
    /// depths, and this walks it against whatever the tide is now. A few
    /// thousand comparisons, which is nothing next to the re-upload that
    /// keeping it in step used to cost.
    pub fn clearance_tally(&self) -> Tally {
        let mut tally = Tally::default();
        let Some(check) = self.draft_check else {
            return tally;
        };
        for &charted in &self.judged {
            tally.count(depth::assess(&check.draft, charted, &check.tide));
        }
        tally
    }

    /// Replace the temperature wash.
    ///
    /// Each patch becomes two triangles. Uploaded on change rather than
    /// per frame: the lattice is geographic, so panning only moves the
    /// uniform — the same arrangement as the chart's own geometry.
    ///
    /// The depth attribute is deliberately absent from these vertices:
    /// air temperature is not a sounding, and the under-keel check must
    /// not paint the weather red.
    pub fn set_temperature(&mut self, patches: &[FieldPatch]) {
        self.temperature.count = 0;
        if patches.is_empty() {
            return;
        }

        // Its own reference point, from the patches themselves, so the f32
        // offsets stay small at high zoom.
        let origin = centroid_of(patches.iter().flat_map(|p| p.corners));
        let offset = |p: chart::WorldXY| [(p.x - origin.x) as f32, (p.y - origin.y) as f32];

        let mut vertices: Vec<Vertex> = Vec::with_capacity(patches.len() * 6);
        for patch in patches {
            // North-west, north-east, south-east, south-west. Split along
            // the north-west to south-east diagonal.
            for &(a, b, c) in &[(0, 1, 2), (0, 2, 3)] {
                for index in [a, b, c] {
                    vertices.push(Vertex::plain(
                        offset(patch.corners[index]),
                        patch.colours[index],
                    ));
                }
            }
        }

        self.temperature.origin = origin;
        self.temperature.count = vertices.len() as u32;
        if vertices.len() as u64 > self.temperature.capacity {
            let capacity = (vertices.len() as u64).next_power_of_two();
            log::debug!("growing temperature buffer to {capacity} vertices");
            self.temperature.vertices = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("temperature"),
                size: std::mem::size_of::<Vertex>() as u64 * capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.temperature.capacity = capacity;
        }
        self.queue.write_buffer(
            &self.temperature.vertices,
            0,
            bytemuck::cast_slice(&vertices),
        );
    }

    /// Triangles currently in the temperature wash.
    pub fn temperature_triangle_count(&self) -> u32 {
        self.temperature.count / 3
    }

    pub fn set_chart(
        &mut self,
        areas: &[StyledArea],
        lines: &[StyledLine],
        points: &[StyledPoint],
    ) {
        let mut vertices: Vec<Vertex> = Vec::new();

        let origin = centroid(lines, points);

        let offset = |p: chart::WorldXY| [(p.x - origin.x) as f32, (p.y - origin.y) as f32];

        // Caller order is draw order: rutter-charts sorts by depth_order so
        // land and depth areas arrive before the marks that sit on them.
        // Preserving it here is the whole reason this takes a slice rather
        // than something unordered.
        //
        // The verdict itself is *not* computed here any more. A vertex
        // carries its charted depth and the shader decides, so that moving
        // the tide costs a uniform write rather than a re-upload of
        // 413,000 vertices. What is kept is the list of depths the panel's
        // tally counts, which is recomputed on demand instead.
        self.judged.clear();

        // Fills go in their own buffer but share this origin, so one pan
        // updates both uniforms consistently. Getting that wrong would
        // slide the shading off its own coastline.
        let mut fill_vertices: Vec<Vertex> = Vec::new();
        for area in areas {
            let judgeable = judgeable(area.style);
            // A fill is emitted whenever it has *either* a band colour or a
            // depth the check could shade. Skipping the ones with no band
            // colour would mean a depth area vanishing when the check was
            // switched off, which is the wrong failure — and deciding it
            // here is the only part that cannot move to the shader,
            // because a triangle that was never uploaded cannot appear.
            let band = fill_colour_for(area.style, area.value);
            if band.is_none() && !(judgeable && area.value.is_some()) {
                continue;
            }
            // Transparent rather than skipped: the shader will paint it if
            // the verdict says so, and leave it invisible if not.
            let colour = band.unwrap_or([0.0, 0.0, 0.0, 0.0]);
            let depth = judgeable.then_some(area.value).flatten();
            for triangle in &area.triangles {
                for corner in triangle {
                    fill_vertices.push(Vertex::judged(offset(*corner), colour, depth));
                }
            }
        }

        for line in lines {
            let colour = colour_for(line.style);
            let depth = judgeable(line.style).then_some(line.value).flatten();
            if let Some(d) = depth {
                // Remembered so the panel's tally can be recomputed when
                // the tide moves. Depth areas are the picture; contours are
                // the same information as a boundary. Both are counted, so
                // the tally is of features judged rather than of dots.
                self.judged.push(d);
            }
            // A polyline of n points is n-1 separate segments under
            // LineList. A line strip topology would halve this, but it
            // cannot express several disconnected polylines in one draw.
            for pair in line.points.windows(2) {
                vertices.push(Vertex::judged(offset(pair[0]), colour, depth));
                vertices.push(Vertex::judged(offset(pair[1]), colour, depth));
            }
        }

        // Marks as small crosses. Keeping them a constant size on screen is
        // not possible here — this buffer does not know the zoom, and that
        // is deliberate, because it is reused across zooms without being
        // rewritten. Size them against the data's own extent instead, then
        // scale per style so a danger is bigger than a sounding.
        let arm = point_arm(lines, points);
        for p in points {
            let c = offset(p.at);
            let colour = colour_for(p.style);
            let depth = judgeable(p.style).then_some(p.value).flatten();
            if let Some(d) = depth {
                self.judged.push(d);
            }
            // The arm goes in its own attribute and is scaled on the GPU,
            // which is what lets a verdict change the size of a mark
            // without the geometry being rebuilt. Emphasis is modest now
            // that the area fills carry the signal — the crosses were 2.2x
            // when they were the only way to see a verdict — but a
            // sounding is still the more precise statement, since one 1.2 m
            // spot inside an 1.8-3.6 m band is invisible in the band's own
            // colour. So the size still says something.
            let a = arm * mark_scale(p.style);
            for (dx, dy) in [(a, 0.0), (0.0, a)] {
                vertices.push(Vertex::judged(c, colour, depth).with_arm([-dx, -dy]));
                vertices.push(Vertex::judged(c, colour, depth).with_arm([dx, dy]));
            }
        }

        self.charts.origin = origin;
        self.charts.count = vertices.len() as u32;

        self.fills.origin = origin;
        self.fills.count = fill_vertices.len() as u32;
        if !fill_vertices.is_empty() {
            if fill_vertices.len() as u64 > self.fills.capacity {
                let capacity = (fill_vertices.len() as u64).next_power_of_two();
                log::debug!("growing fill buffer to {capacity} vertices");
                self.fills = Layer::new(&self.device, &self.bind_layout, "fills", capacity);
                self.fills.origin = origin;
                self.fills.count = fill_vertices.len() as u32;
            }
            self.queue.write_buffer(
                &self.fills.vertices,
                0,
                bytemuck::cast_slice(&fill_vertices),
            );
        }

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

    /// Replace the aerial imagery.
    ///
    /// Call when the set of visible imagery tiles changes, not per frame:
    /// each tile becomes a GPU texture, and re-uploading a hundred
    /// quarter-megabyte textures every frame would be ruinous.
    ///
    /// Tiles already on the GPU are kept. That is the point of keying them
    /// by [`TileId`]: panning typically changes a row or a column, and
    /// re-uploading the twenty tiles that did not move is pure waste.
    pub fn set_imagery(&mut self, tiles: &[chart::TileImage]) {
        // Drop textures no longer wanted, keep the rest.
        let wanted: Vec<TileId> = tiles.iter().map(|t| t.id).collect();
        self.imagery_tiles.retain(|t| wanted.contains(&t.id));

        let mut vertices: Vec<ImageryVertex> = Vec::with_capacity(tiles.len() * 6);
        let origin = imagery_origin(tiles);
        let mut placed: Vec<ImageryTile> = Vec::with_capacity(tiles.len());

        for tile in tiles {
            if !tile.is_consistent() {
                log::warn!(
                    "imagery {:?}: {} bytes for {}x{}, skipping",
                    tile.id,
                    tile.rgba.len(),
                    tile.width,
                    tile.height
                );
                continue;
            }

            let first_vertex = vertices.len() as u32;

            // The tile's corners in world units, then as offsets from the
            // layer's reference point.
            let n = f64::from(TileId::grid_size(tile.id.z));
            let (x0, y0) = (f64::from(tile.id.x) / n, f64::from(tile.id.y) / n);
            let (x1, y1) = (f64::from(tile.id.x + 1) / n, f64::from(tile.id.y + 1) / n);

            let corner = |x: f64, y: f64, u: f32, v: f32| ImageryVertex {
                offset: [(x - origin.x) as f32, (y - origin.y) as f32],
                uv: [u, v],
            };

            // Two triangles. v=0 at the tile's north edge, because image
            // rows run top-down and so does world y — the one place those
            // two conventions agree, and worth saying so since everywhere
            // else they fight.
            let (nw, ne) = (corner(x0, y0, 0.0, 0.0), corner(x1, y0, 1.0, 0.0));
            let (sw, se) = (corner(x0, y1, 0.0, 1.0), corner(x1, y1, 1.0, 1.0));
            vertices.extend_from_slice(&[nw, ne, se, nw, se, sw]);

            // Reuse the texture if this tile is already resident.
            if let Some(existing) = self.imagery_tiles.iter().find(|t| t.id == tile.id) {
                placed.push(ImageryTile {
                    id: tile.id,
                    bind_group: existing.bind_group.clone(),
                    first_vertex,
                });
                continue;
            }

            placed.push(ImageryTile {
                id: tile.id,
                bind_group: self.upload_tile(tile),
                first_vertex,
            });
        }

        self.imagery_origin = origin;
        self.imagery_tiles = placed;

        if vertices.is_empty() {
            return;
        }

        let needed = vertices.len() as u64;
        if needed > self.imagery_capacity {
            let capacity = needed.next_power_of_two();
            log::debug!("growing imagery buffer to {capacity} vertices");
            self.imagery_vertices = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("imagery"),
                size: capacity * std::mem::size_of::<ImageryVertex>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.imagery_capacity = capacity;
        }

        self.queue
            .write_buffer(&self.imagery_vertices, 0, bytemuck::cast_slice(&vertices));
    }

    /// Create a texture and binding for one decoded tile.
    fn upload_tile(&self, tile: &chart::TileImage) -> wgpu::BindGroup {
        let size = wgpu::Extent3d {
            width: tile.width,
            height: tile.height,
            depth_or_array_layers: 1,
        };

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("imagery tile"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Srgb, because the surface is sRGB and the photograph's bytes
            // are gamma-encoded. Using the linear format here makes aerial
            // imagery look washed out, which is a subtle enough wrongness
            // to live with for months.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        self.queue.write_texture(
            texture.as_image_copy(),
            &tile.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(tile.width * 4),
                rows_per_image: Some(tile.height),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("imagery tile"),
            layout: &self.imagery_texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.imagery_sampler),
                },
            ],
        })
    }

    /// Drop the imagery, freeing its textures.
    pub fn clear_imagery(&mut self) {
        self.imagery_tiles.clear();
    }

    /// Imagery tiles currently resident on the GPU.
    pub fn imagery_tile_count(&self) -> usize {
        self.imagery_tiles.len()
    }

    /// How strongly imagery shows through, clamped to 0..1.
    pub fn set_imagery_opacity(&mut self, opacity: f32) {
        self.imagery_opacity = opacity.clamp(0.0, 1.0);
    }

    pub fn imagery_opacity(&self) -> f32 {
        self.imagery_opacity
    }

    /// Replace the overlay marks.
    ///
    /// Uploaded once per change, not per frame: a mark's pixel offsets never
    /// change, and panning only moves the uniform — the same arrangement
    /// that makes the chart's own geometry cheap to drag.
    pub fn set_overlay(&mut self, marks: &[Overlay]) {
        /// Segments in a ring. Twenty is smooth at the sizes these are
        /// drawn — a six-pixel circle — and 240 of them is still nothing.
        const RING_SEGMENTS: usize = 20;

        if marks.is_empty() {
            self.overlay_count = 0;
            self.overlay_fill_count = 0;
            return;
        }

        // One reference point for the lot, so that a pan is one uniform
        // write. The anchors are small offsets from it, which is what keeps
        // them inside f32 at high zoom.
        let origin = centroid_of(marks.iter().map(Overlay::at));
        let mut vertices: Vec<OverlayVertex> = Vec::new();
        let mut fill_vertices: Vec<OverlayVertex> = Vec::new();

        for mark in marks {
            let anchor = [
                (mark.at().x - origin.x) as f32,
                (mark.at().y - origin.y) as f32,
            ];
            let mut segment = |a: [f32; 2], b: [f32; 2], colour: [f32; 4]| {
                vertices.push(OverlayVertex {
                    anchor,
                    pixel: a,
                    colour,
                });
                vertices.push(OverlayVertex {
                    anchor,
                    pixel: b,
                    colour,
                });
            };
            let mut triangle = |a: [f32; 2], b: [f32; 2], c: [f32; 2], colour: [f32; 4]| {
                for pixel in [a, b, c] {
                    fill_vertices.push(OverlayVertex {
                        anchor,
                        pixel,
                        colour,
                    });
                }
            };

            match *mark {
                Overlay::Ring {
                    radius_px,
                    emphasised,
                    colour,
                    ..
                } => {
                    let ring = |segment: &mut dyn FnMut([f32; 2], [f32; 2], [f32; 4]), r: f32| {
                        for i in 0..RING_SEGMENTS {
                            let a = i as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            let b = (i + 1) as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            segment(
                                [r * a.cos(), r * a.sin()],
                                [r * b.cos(), r * b.sin()],
                                colour,
                            );
                        }
                    };
                    ring(&mut segment, radius_px);
                    if emphasised {
                        // A second ring inside, rather than a different
                        // colour: colour already means something here.
                        ring(&mut segment, radius_px * 0.45);
                    }
                }

                // World space at both ends, so the leg stretches with
                // the chart. Pushed directly rather than through
                // `segment`, which anchors every vertex of a mark at one
                // point — right for a symbol, wrong for a line sixty
                // miles long.
                Overlay::Leg { from, to, colour } => {
                    for end in [from, to] {
                        vertices.push(OverlayVertex {
                            anchor: [(end.x - origin.x) as f32, (end.y - origin.y) as f32],
                            pixel: [0.0, 0.0],
                            colour,
                        });
                    }
                }

                Overlay::Waypoint { active, colour, .. } => {
                    /// Half-width of the diamond, pixels.
                    const ARM: f32 = 7.0;

                    // A diamond rather than a circle: a tide station is
                    // already a ring and a waypoint has to be
                    // distinguishable from one at a glance, in the same
                    // colour family as the leg it belongs to.
                    let points = [[0.0, -ARM], [ARM, 0.0], [0.0, ARM], [-ARM, 0.0]];
                    for pair in 0..4 {
                        segment(points[pair], points[(pair + 1) % 4], colour);
                    }

                    if active {
                        // The one being steered for. A ring outside the
                        // diamond, so which mark the bearing and the
                        // distance refer to is never in doubt.
                        for i in 0..RING_SEGMENTS {
                            let a = i as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            let b = (i + 1) as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            segment(
                                [11.0 * a.cos(), 11.0 * a.sin()],
                                [11.0 * b.cos(), 11.0 * b.sin()],
                                colour,
                            );
                        }
                    }
                }

                Overlay::Current {
                    speed_kn,
                    toward_deg,
                    colour,
                    ..
                } => {
                    /// Half-width of the arrowhead, pixels.
                    const HEAD: f32 = 5.0;
                    /// How far back from the point the barbs start.
                    const HEAD_BACK: f32 = 9.0;

                    if speed_kn
                        .partial_cmp(&SLACK_KN)
                        .is_none_or(|o| o == core::cmp::Ordering::Less)
                    {
                        // Slack water. A small open circle, the same way a
                        // calm wind is drawn, because an arrow of no length
                        // pointing nowhere would still look like a claim.
                        for i in 0..RING_SEGMENTS {
                            let a = i as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            let b = (i + 1) as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            segment(
                                [4.0 * a.cos(), 4.0 * a.sin()],
                                [4.0 * b.cos(), 4.0 * b.sin()],
                                colour,
                            );
                        }
                        continue;
                    }

                    // `shaft` gives the unit vector for a bearing in
                    // screen pixels. A current names where it is going, so
                    // the arrow runs from the station outwards along it.
                    let along = shaft(toward_deg);
                    let across = (along.1, -along.0);
                    let point =
                        |d: f32, w: f32| [along.0 * d + across.0 * w, along.1 * d + across.1 * w];

                    let length = current_length_px(speed_kn);
                    let tip = point(length, 0.0);

                    // Drawn from the station, not centred on it: the
                    // station is where the current was measured, and an
                    // arrow straddling it would put half the flow
                    // upstream of the place it describes.
                    segment([0.0, 0.0], tip, colour);
                    segment(tip, point(length - HEAD_BACK, HEAD), colour);
                    segment(tip, point(length - HEAD_BACK, -HEAD), colour);
                }

                Overlay::Boat {
                    course_deg, colour, ..
                } => {
                    // Half again the size it started at, and solid. The
                    // first version was a 13-pixel hollow outline, which
                    // on a chart of ten thousand soundings was genuinely
                    // hard to find — the one mark that must never be hunted
                    // for. Filled, outlined and larger, in that order of
                    // how much it helped.
                    /// Bow, forward of the fix, pixels.
                    const BOW: f32 = 19.0;
                    /// Quarters: abaft the fix, and out.
                    const QUARTER_AFT: f32 = -10.0;
                    const BEAM: f32 = 7.5;
                    /// The transom's notch, which is what makes the stern
                    /// read as a stern rather than as a second bow.
                    const TRANSOM: f32 = -6.0;
                    /// Radius of the no-course mark.
                    const DR_RING: f32 = 8.0;
                    /// Reach of its cross.
                    const DR_CROSS: f32 = 12.0;

                    // These are *physical* pixels, so on a phone at 2.6
                    // pixels per point the whole mark came out about a
                    // third the size it is on a desktop — the one symbol
                    // that must never be hunted for, and the hardest to
                    // see. The platform sets the scale; see
                    // `Renderer::set_boat_scale`.
                    let k = self.boat_scale;
                    let (bow, quarter_aft, beam, transom) =
                        (BOW * k, QUARTER_AFT * k, BEAM * k, TRANSOM * k);
                    let (dr_ring, dr_cross) = (DR_RING * k, DR_CROSS * k);

                    // Black, at the hull's own alpha. The outline is what
                    // separates the boat from whatever it is over — orange
                    // on an orange roof, or on the amber of a
                    // tight-at-low-water shading, would otherwise
                    // disappear exactly where the boat matters most.
                    let edge = [0.05, 0.05, 0.08, colour[3]];

                    let Some(course) = course_deg else {
                        // No course and no heading: a ringed cross, the
                        // chart convention for a position without a
                        // vector. Filled too, so it is as findable as the
                        // hull it stands in for.
                        let ring = |i: usize| {
                            let a = i as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            [dr_ring * a.cos(), dr_ring * a.sin()]
                        };
                        for i in 0..RING_SEGMENTS {
                            triangle([0.0, 0.0], ring(i), ring(i + 1), colour);
                            segment(ring(i), ring(i + 1), edge);
                        }
                        segment([-dr_cross, 0.0], [dr_cross, 0.0], edge);
                        segment([0.0, -dr_cross], [0.0, dr_cross], edge);
                        continue;
                    };

                    // The same unit vector a barb's shaft uses: a bearing
                    // clockwise from north, in a frame where y grows down.
                    // A course is a direction of travel rather than a
                    // direction of origin, but the arithmetic is identical.
                    let along = shaft(course);
                    let across = (along.1, -along.0);
                    let point =
                        |d: f32, w: f32| [along.0 * d + across.0 * w, along.1 * d + across.1 * w];

                    let bow = point(bow, 0.0);
                    let port = point(quarter_aft, -beam);
                    let starboard = point(quarter_aft, beam);
                    let transom = point(transom, 0.0);

                    // Two triangles about the notch, so the concave stern
                    // stays concave. One triangle bow-port-starboard would
                    // fill across it and the hull would read as an arrow
                    // pointing both ways.
                    triangle(bow, port, transom, colour);
                    triangle(bow, transom, starboard, colour);

                    segment(bow, port, edge);
                    segment(port, transom, edge);
                    segment(transom, starboard, edge);
                    segment(starboard, bow, edge);
                }

                Overlay::Barb {
                    speed_kn,
                    from_deg,
                    colour,
                    ..
                } => {
                    /// Shaft length, pixels.
                    const SHAFT: f32 = 24.0;
                    /// A full barb; a half barb is half this.
                    const BARB: f32 = 9.0;
                    /// Spacing of the feathers along the shaft.
                    const STEP: f32 = 4.5;
                    if feathers(speed_kn)
                        == (Feathers {
                            pennants: 0,
                            full: 0,
                            half: false,
                        })
                    {
                        for i in 0..RING_SEGMENTS {
                            let a = i as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            let b = (i + 1) as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                            segment(
                                [3.0 * a.cos(), 3.0 * a.sin()],
                                [3.0 * b.cos(), 3.0 * b.sin()],
                                colour,
                            );
                        }
                        continue;
                    }

                    let along = shaft(from_deg);
                    let across = (along.1, -along.0);
                    let point =
                        |d: f32, w: f32| [along.0 * d + across.0 * w, along.1 * d + across.1 * w];

                    segment([0.0, 0.0], point(SHAFT, 0.0), colour);

                    // Feathers from the tip inwards, the order they read in.
                    let Feathers {
                        pennants,
                        full,
                        half,
                    } = feathers(speed_kn);
                    let mut offset = 0.0;

                    for _ in 0..pennants {
                        // A pennant, drawn as an outline since this pipeline
                        // draws lines: out from the shaft, then back to it.
                        let base = SHAFT - offset;
                        segment(point(base, 0.0), point(base, BARB), colour);
                        segment(point(base, BARB), point(base - STEP * 2.0, 0.0), colour);
                        offset += STEP * 2.5;
                    }
                    for _ in 0..full {
                        let base = SHAFT - offset;
                        segment(point(base, 0.0), point(base, BARB), colour);
                        offset += STEP;
                    }
                    if half {
                        // Never at the very tip: on a five-knot wind a half
                        // barb there reads as a full barb seen end-on.
                        let base = SHAFT - offset.max(STEP);
                        segment(point(base, 0.0), point(base, BARB / 2.0), colour);
                    }
                }
            }
        }

        self.overlay_origin = origin;
        self.overlay_count = vertices.len() as u32;
        self.overlay_fill_count = fill_vertices.len() as u32;

        if !fill_vertices.is_empty() {
            if fill_vertices.len() as u64 > self.overlay_fill_capacity {
                let capacity = (fill_vertices.len() as u64).next_power_of_two();
                log::debug!("growing overlay fill buffer to {capacity} vertices");
                self.overlay_fill_vertices = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("overlay fills"),
                    size: std::mem::size_of::<OverlayVertex>() as u64 * capacity,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.overlay_fill_capacity = capacity;
            }
            self.queue.write_buffer(
                &self.overlay_fill_vertices,
                0,
                bytemuck::cast_slice(&fill_vertices),
            );
        }

        if vertices.is_empty() {
            return;
        }

        if vertices.len() as u64 > self.overlay_capacity {
            let capacity = (vertices.len() as u64).next_power_of_two();
            log::debug!("growing overlay buffer to {capacity} vertices");
            self.overlay_vertices = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("overlay"),
                size: std::mem::size_of::<OverlayVertex>() as u64 * capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.overlay_capacity = capacity;
        }
        self.queue
            .write_buffer(&self.overlay_vertices, 0, bytemuck::cast_slice(&vertices));
    }

    /// Marks currently in the overlay buffer, as line segments.
    pub fn overlay_segment_count(&self) -> u32 {
        self.overlay_count / 2
    }

    /// Solid overlay geometry, as triangles. The boat's hull and nothing
    /// else, so a zero here with a fix on screen means the boat is being
    /// drawn hollow.
    pub fn overlay_fill_triangle_count(&self) -> u32 {
        self.overlay_fill_count / 3
    }

    /// Triangles currently held in the fill buffer.
    pub fn fill_triangle_count(&self) -> u32 {
        self.fills.count / 3
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

        // The grid is not chart data, so it carries no depth and the
        // under-keel check must never touch it.
        let line = |out: &mut Vec<Vertex>, ax: f64, ay: f64, bx: f64, by: f64, colour| {
            out.push(Vertex::plain(
                [(ax - centre.x) as f32, (ay - centre.y) as f32],
                colour,
            ));
            out.push(Vertex::plain(
                [(bx - centre.x) as f32, (by - centre.y) as f32],
                colour,
            ));
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

/// Mean of a set of positions, used as a layer's reference point.
///
/// The same idea as [`centroid`], over anything that yields positions.
fn centroid_of(positions: impl Iterator<Item = chart::WorldXY>) -> chart::WorldXY {
    let mut sum = (0.0f64, 0.0f64);
    let mut n = 0u64;
    for p in positions {
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

/// Mean of every vertex, used as a layer's reference point.
///
/// The mean rather than the bounding-box centre: it is cheaper, it needs no
/// special case for empty input, and the only property that matters is being
/// *near* the data so the f32 offsets stay small.
fn centroid(lines: &[StyledLine], points: &[StyledPoint]) -> chart::WorldXY {
    let mut sum = (0.0f64, 0.0f64);
    let mut n = 0u64;

    for p in lines
        .iter()
        .flat_map(|l| l.points.iter())
        .chain(points.iter().map(|p| &p.at))
    {
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
fn point_arm(lines: &[StyledLine], points: &[StyledPoint]) -> f32 {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);

    for p in lines
        .iter()
        .flat_map(|l| l.points.iter())
        .chain(points.iter().map(|p| &p.at))
    {
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

    fn line(points: Vec<WorldXY>) -> StyledLine {
        StyledLine {
            style: ChartStyle::Coastline,
            layer: 0,
            points,
            value: None,
        }
    }

    fn point(at: WorldXY) -> StyledPoint {
        StyledPoint {
            style: ChartStyle::Sounding,
            layer: 0,
            at,
            value: None,
        }
    }

    #[test]
    fn five_knot_steps_decompose_the_way_a_navigator_reads_them() {
        let f = |kn: f32| feathers(kn);
        let barb = |pennants, full, half| Feathers {
            pennants,
            full,
            half,
        };

        // Calm: no shaft at all, so no feathers.
        assert_eq!(f(0.0), barb(0, 0, false));
        assert_eq!(f(2.0), barb(0, 0, false));

        assert_eq!(f(5.0), barb(0, 0, true), "one half barb");
        assert_eq!(f(10.0), barb(0, 1, false), "one full barb");
        assert_eq!(f(15.0), barb(0, 1, true));
        assert_eq!(f(25.0), barb(0, 2, true));
        assert_eq!(f(45.0), barb(0, 4, true));
        assert_eq!(f(50.0), barb(1, 0, false), "a pennant, not five barbs");
        assert_eq!(f(65.0), barb(1, 1, true));
        assert_eq!(f(100.0), barb(2, 0, false));
        assert_eq!(f(105.0), barb(2, 0, true));
    }

    #[test]
    fn a_barb_is_rounded_to_the_nearest_five_knots() {
        // The symbol has no finer resolution, so 12 knots reads as 10 and
        // 13 as 15. Rounding up at 2.5 is the convention.
        assert_eq!(feathers(12.0).full, 1);
        assert!(!feathers(12.0).half);
        assert_eq!(feathers(13.0).full, 1);
        assert!(feathers(13.0).half);
    }

    #[test]
    fn feathers_always_add_up_to_the_speed_they_came_from() {
        // The property that matters: whatever the decomposition, reading it
        // back must give the rounded speed. An off-by-one in the division
        // would pass the specific cases above and fail somewhere between.
        for tenths in 25..2000u32 {
            let kn = tenths as f32 / 10.0;
            let f = feathers(kn);
            let read_back = f64::from(f.pennants) * 50.0
                + f64::from(f.full) * 10.0
                + if f.half { 5.0 } else { 0.0 };
            let rounded = f64::from((kn / 5.0).round() * 5.0);
            assert!(
                (read_back - rounded).abs() < 1e-9,
                "{kn} kn decomposed to {f:?}, which reads as {read_back}, not {rounded}"
            );
        }
    }

    #[test]
    fn nonsense_speeds_do_not_produce_nonsense_barbs() {
        // A grid can carry a fill value or a NaN where a model has no data.
        for kn in [f32::NAN, f32::INFINITY, -10.0, -0.0] {
            let f = feathers(kn);
            if kn.is_infinite() {
                // Clamped rather than overflowing into a wrapped count.
                assert!(f.pennants > 0);
            } else {
                assert_eq!(
                    f,
                    Feathers {
                        pennants: 0,
                        full: 0,
                        half: false
                    },
                    "{kn} should give no feathers"
                );
            }
        }
    }

    #[test]
    fn the_shaft_points_into_the_wind() {
        // The direction a wind is named by. Screen y grows downward, so a
        // northerly — from 000 — runs *up* the screen, which is negative y.
        let approx = |a: (f32, f32), b: (f32, f32)| {
            assert!(
                (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6,
                "expected {b:?}, got {a:?}"
            );
        };
        // A northerly points up the screen.
        approx(shaft(0.0), (0.0, -1.0));
        approx(shaft(90.0), (1.0, 0.0));
        approx(shaft(180.0), (0.0, 1.0));
        approx(shaft(270.0), (-1.0, 0.0));
    }

    #[test]
    fn centroid_of_nothing_is_the_origin() {
        let c = centroid(&[], &[]);
        assert_eq!((c.x, c.y), (0.0, 0.0));
    }

    #[test]
    fn centroid_averages_every_vertex() {
        let l = line(vec![w(0.0, 0.0), w(1.0, 1.0)]);
        let c = centroid(&[l], &[point(w(2.0, 2.0))]);
        assert!((c.x - 1.0).abs() < 1e-12, "x was {}", c.x);
        assert!((c.y - 1.0).abs() < 1e-12, "y was {}", c.y);
    }

    #[test]
    fn centroid_counts_lines_and_points_alike() {
        // Two points on one side, one on the other: the mean must lean.
        let l = line(vec![w(0.0, 0.0), w(0.0, 0.0)]);
        let c = centroid(&[l], &[point(w(0.3, 0.0))]);
        assert!((c.x - 0.1).abs() < 1e-12, "x was {}", c.x);
    }

    #[test]
    fn centroid_stays_near_the_data_far_from_the_origin() {
        // The whole point of a reference point: offsets from it must be
        // small even when the data sits in a corner of the world where f32
        // has no precision left.
        let pts = vec![w(0.999_999_0, 0.999_999_0), w(0.999_999_5, 0.999_999_5)];
        let c = centroid(&[line(pts.clone())], &[]);

        for p in pts {
            let offset = ((p.x - c.x) as f32, (p.y - c.y) as f32);
            assert!(offset.0.abs() < 1e-5, "offset {offset:?} is not small");
            // And the offset must survive f32 without collapsing to zero,
            // which is what happens if you store absolute coordinates.
            assert_ne!(offset.0, 0.0);
        }
    }

    #[test]
    fn point_arm_scales_with_the_data_extent() {
        let small = line(vec![w(0.5, 0.5), w(0.501, 0.501)]);
        let large = line(vec![w(0.0, 0.0), w(1.0, 1.0)]);
        assert!(
            point_arm(&[small], &[]) < point_arm(&[large], &[]),
            "a tighter chart should get smaller marks"
        );
    }

    #[test]
    fn point_arm_is_positive_for_degenerate_input() {
        // A single point has no extent; the mark must still be drawable.
        assert!(point_arm(&[], &[point(w(0.5, 0.5))]) > 0.0);
        assert!(point_arm(&[], &[]) > 0.0);
        // A vertical line has zero width but real height.
        let vertical = line(vec![w(0.5, 0.4), w(0.5, 0.6)]);
        assert!(point_arm(&[vertical], &[]) > 0.0);
    }

    #[test]
    fn point_arm_is_finite() {
        let l = line(vec![w(0.0, 0.0), w(1.0, 1.0)]);
        assert!(point_arm(&[l], &[point(w(0.5, 0.5))]).is_finite());
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;

    /// Every style must be visually distinguishable from every other.
    ///
    /// Not aesthetics — a chart where a danger and a depth contour are the
    /// same colour is a chart that can get you wrecked. This is the cheapest
    /// possible guard against a copy-paste in the palette.
    #[test]
    fn no_two_styles_share_a_colour() {
        let styles = [
            ChartStyle::Coastline,
            ChartStyle::Land,
            ChartStyle::DepthArea,
            ChartStyle::DepthContour,
            ChartStyle::Sounding,
            ChartStyle::Buoy,
            ChartStyle::Beacon,
            ChartStyle::Light,
            ChartStyle::Danger,
            ChartStyle::Area,
            ChartStyle::Other,
        ];

        for (i, a) in styles.iter().enumerate() {
            for b in &styles[i + 1..] {
                assert_ne!(
                    colour_for(*a),
                    colour_for(*b),
                    "{a:?} and {b:?} are the same colour"
                );
            }
        }
    }

    #[test]
    fn every_colour_is_opaque_and_in_range() {
        for style in [
            ChartStyle::Coastline,
            ChartStyle::Land,
            ChartStyle::DepthArea,
            ChartStyle::DepthContour,
            ChartStyle::Sounding,
            ChartStyle::Buoy,
            ChartStyle::Beacon,
            ChartStyle::Light,
            ChartStyle::Danger,
            ChartStyle::Area,
            ChartStyle::Other,
        ] {
            let c = colour_for(style);
            for channel in c {
                assert!(
                    (0.0..=1.0).contains(&channel),
                    "{style:?} has an out-of-range channel in {c:?}"
                );
            }
            assert_eq!(c[3], 1.0, "{style:?} should be opaque");
        }
    }

    #[test]
    fn dangers_are_drawn_larger_than_soundings() {
        // A chart has hundreds of soundings and a handful of dangers; the
        // dangers are the ones that must catch the eye.
        assert!(mark_scale(ChartStyle::Danger) > mark_scale(ChartStyle::Sounding));
        assert!(mark_scale(ChartStyle::Buoy) > mark_scale(ChartStyle::Sounding));
    }

    #[test]
    fn every_mark_scale_is_positive() {
        for style in [
            ChartStyle::Sounding,
            ChartStyle::Buoy,
            ChartStyle::Beacon,
            ChartStyle::Light,
            ChartStyle::Danger,
            ChartStyle::Other,
        ] {
            assert!(mark_scale(style) > 0.0, "{style:?} would be invisible");
        }
    }

    // ── The under-keel check, now that the shader does it ───────────────
    //
    // Two implementations of a navigation rule is a real risk, and the
    // ordinary way it goes wrong is not someone writing bad WGSL — it is
    // someone changing `rutter_core::depth::assess` and never thinking
    // about the shader again. These two tests exist to make that fail
    // loudly instead of quietly painting the wrong water green.

    /// `chart.wgsl`, read at compile time so a test can check it.
    const SHADER: &str = include_str!("chart.wgsl");

    /// Pull `const NAME = vec4<f32>(a, b, c, d);` out of the shader.
    fn shader_colour(name: &str) -> [f32; 4] {
        let line = SHADER
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with(&format!("const {name} ")))
            .unwrap_or_else(|| panic!("{name} is not declared in chart.wgsl"));
        let inside = line
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(args, _)| args)
            .unwrap_or_else(|| panic!("{name} is not a vec4 literal: {line}"));
        let parts: Vec<f32> = inside
            .split(',')
            .map(|v| v.trim().parse().unwrap_or_else(|_| panic!("{name}: {v}")))
            .collect();
        assert_eq!(parts.len(), 4, "{name} has {} components", parts.len());
        [parts[0], parts[1], parts[2], parts[3]]
    }

    /// Pull a scalar `const NAME: f32 = value;` out of the shader.
    fn shader_scalar(name: &str) -> f32 {
        let line = SHADER
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with(&format!("const {name}:")))
            .unwrap_or_else(|| panic!("{name} is not declared in chart.wgsl"));
        line.split_once('=')
            .map(|(_, v)| v.trim().trim_end_matches(';'))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{name} is not a scalar literal: {line}"))
    }

    #[test]
    fn the_shader_palette_matches_the_rust_one() {
        // Colour is the whole output of this feature. A shader constant
        // edited to a slightly different amber, or a Rust one changed
        // without the shader, would show as a chart that disagrees with
        // its own legend — and nothing else would complain.
        assert_eq!(
            shader_colour("CLEAR_LINE"),
            colour_for_clearance(Clearance::Clear)
        );
        assert_eq!(
            shader_colour("TIGHT_LINE"),
            colour_for_clearance(Clearance::TightAtLow)
        );
        assert_eq!(
            shader_colour("FOUL_LINE"),
            colour_for_clearance(Clearance::Foul)
        );

        assert_eq!(
            Some(shader_colour("TIGHT_FILL")),
            clearance_fill_for(Clearance::TightAtLow)
        );
        assert_eq!(
            Some(shader_colour("FOUL_FILL")),
            clearance_fill_for(Clearance::Foul)
        );
        // Clear water is unshaded, which in a shader means transparent
        // rather than absent.
        assert_eq!(clearance_fill_for(Clearance::Clear), None);
        assert_eq!(shader_colour("CLEAR_FILL"), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn the_no_depth_sentinel_matches_the_shader() {
        // If these drift apart the shader either judges features that are
        // not depths — painting a light's height as foul water — or stops
        // judging real soundings. Both are silent.
        assert_eq!(shader_scalar("NO_DEPTH"), NO_DEPTH);
        // And it has to be below any depth a chart could really state.
        // The Challenger Deep is 10,935 m, so a million metres is four
        // orders of magnitude of headroom. A constant assertion, which is
        // the point: it documents the requirement and fails the build if
        // someone brings the sentinel up into the range of real data.
        const _: () = assert!(NO_DEPTH < -1.0e6);
    }

    /// A literal transliteration of `verdict_of` in `chart.wgsl`.
    ///
    /// Kept beside the shader it copies, and compared against the core
    /// rule by the test below. This does not catch both being edited
    /// wrongly in the same way — but it does catch the failure that
    /// actually happens, which is `assess` changing and the shader not.
    fn shader_verdict(depth: f32, check: [f32; 4]) -> Option<Clearance> {
        if check[0] == 0.0 || depth <= NO_DEPTH {
            return None;
        }
        let needed = check[1] + shader_scalar("TIE_MARGIN");
        if depth + check[2] < needed {
            return Some(Clearance::Foul);
        }
        if depth + check[3] < needed {
            return Some(Clearance::TightAtLow);
        }
        Some(Clearance::Clear)
    }

    #[test]
    fn clearance_matches_the_core_rule() {
        // Over a grid that covers the cases that matter: drying heights
        // (negative charted depths), the exact boundary where water equals
        // draft, a deep-keeled yacht, a dinghy, and a falling tide.
        let drafts = [
            Draft {
                draft_m: 0.3,
                safety_m: 0.0,
            },
            Draft {
                draft_m: 1.8,
                safety_m: 0.5,
            },
            Draft {
                draft_m: 2.0,
                safety_m: 0.0,
            },
            Draft {
                draft_m: 3.4,
                safety_m: 1.0,
            },
        ];
        let tides = [
            TideWindow {
                now_m: 0.0,
                lowest_m: 0.0,
            },
            TideWindow {
                now_m: 1.7,
                lowest_m: 0.2,
            },
            TideWindow {
                now_m: 0.5,
                lowest_m: -0.4,
            },
            TideWindow {
                now_m: 2.3,
                lowest_m: 2.3,
            },
        ];

        let mut checked = 0;
        for draft in drafts {
            for tide in tides {
                let check = ViewUniform::check_of(Some(DraftCheck { draft, tide }));
                // Tenth-metre steps from well below datum to well below
                // the keel of anything.
                for tenths in -30..300 {
                    let charted = f64::from(tenths) / 10.0;
                    let core = depth::assess(&draft, charted, &tide);
                    let shader = shader_verdict(charted as f32, check)
                        .unwrap_or_else(|| panic!("{charted} m went unjudged"));

                    // How far this case is from a decision boundary. The
                    // two arithmetics can only differ within a millimetre
                    // of one, so away from them they must agree exactly.
                    let needed = draft.needed_m();
                    let slack = (charted + tide.now_m - needed)
                        .abs()
                        .min((charted + tide.lowest_m - needed).abs());

                    if slack > 0.01 {
                        assert_eq!(
                            shader, core,
                            "charted {charted} m, draft {draft:?}, tide {tide:?}"
                        );
                    } else {
                        // At a boundary the shader may be one step more
                        // severe, never less. A depth check that rounds in
                        // the boat's favour is the one failure this whole
                        // feature exists to avoid.
                        assert!(
                            severity(shader) >= severity(core),
                            "shader was {shader:?} where the rule says {core:?}: \
                             charted {charted} m, draft {draft:?}, tide {tide:?}"
                        );
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 5_000, "only {checked} cases");
    }

    /// How bad a verdict is, so "never less severe" can be asserted.
    fn severity(clearance: Clearance) -> u8 {
        match clearance {
            Clearance::Clear => 0,
            Clearance::TightAtLow => 1,
            Clearance::Foul => 2,
        }
    }

    #[test]
    fn a_tie_falls_against_the_boat() {
        // Exactly enough water, to the millimetre. The rule in core calls
        // that clear; the shader calls it foul, deliberately, because f32
        // cannot be trusted to agree with f64 at the boundary and the safe
        // direction to be wrong is the pessimistic one.
        let draft = Draft {
            draft_m: 2.0,
            safety_m: 0.0,
        };
        let tide = TideWindow {
            now_m: 2.0,
            lowest_m: 2.0,
        };
        let check = ViewUniform::check_of(Some(DraftCheck { draft, tide }));

        assert_eq!(depth::assess(&draft, 0.0, &tide), Clearance::Clear);
        assert_eq!(shader_verdict(0.0, check), Some(Clearance::Foul));

        // A centimetre of real clearance and they agree again, so the
        // pessimism is confined to the boundary.
        assert_eq!(shader_verdict(0.01, check), Some(Clearance::Clear));
        assert_eq!(depth::assess(&draft, 0.01, &tide), Clearance::Clear);
    }

    #[test]
    fn the_check_being_off_judges_nothing() {
        let check = ViewUniform::check_of(None);
        for charted in [-2.0_f32, 0.0, 1.0, 50.0] {
            assert_eq!(shader_verdict(charted, check), None);
        }
    }

    #[test]
    fn a_feature_with_no_depth_is_never_judged() {
        // A light's HEIGHT is above datum, not below it. Judging one would
        // paint a lighthouse as foul water.
        let check = ViewUniform::check_of(Some(DraftCheck {
            draft: Draft {
                draft_m: 2.0,
                safety_m: 0.5,
            },
            tide: TideWindow {
                now_m: 0.0,
                lowest_m: 0.0,
            },
        }));
        assert_eq!(shader_verdict(NO_DEPTH, check), None);
        assert_eq!(shader_verdict(NO_DEPTH - 1.0, check), None);
        // And the styles that carry a real depth are the only ones gated in.
        assert!(judgeable(ChartStyle::Sounding));
        assert!(judgeable(ChartStyle::DepthArea));
        assert!(judgeable(ChartStyle::DepthContour));
        assert!(judgeable(ChartStyle::Danger));
        assert!(
            !judgeable(ChartStyle::Light),
            "a light's height is not a depth"
        );
        assert!(!judgeable(ChartStyle::Buoy));
        assert!(!judgeable(ChartStyle::Coastline));
        assert!(!judgeable(ChartStyle::Land));
    }

    #[test]
    fn a_vertex_carries_its_depth_or_says_it_has_none() {
        assert_eq!(Vertex::plain([0.0, 0.0], [1.0; 4]).depth, NO_DEPTH);
        assert_eq!(Vertex::judged([0.0, 0.0], [1.0; 4], None).depth, NO_DEPTH);
        assert_eq!(Vertex::judged([0.0, 0.0], [1.0; 4], Some(4.5)).depth, 4.5);
        // Drying heights are negative and must survive as negatives, not
        // be mistaken for the sentinel.
        let drying = Vertex::judged([0.0, 0.0], [1.0; 4], Some(-1.2));
        assert_eq!(drying.depth, -1.2);
        assert!(drying.depth > NO_DEPTH);
    }

    #[test]
    fn the_arm_is_zero_unless_it_is_set() {
        // Everything but a sounding's cross must be unaffected by the
        // emphasis scaling, or the whole chart would breathe with the tide.
        assert_eq!(Vertex::plain([1.0, 2.0], [1.0; 4]).arm, [0.0, 0.0]);
        assert_eq!(
            Vertex::judged([1.0, 2.0], [1.0; 4], Some(3.0)).arm,
            [0.0, 0.0]
        );
        assert_eq!(
            Vertex::judged([1.0, 2.0], [1.0; 4], Some(3.0))
                .with_arm([0.5, -0.5])
                .arm,
            [0.5, -0.5]
        );
    }
}
