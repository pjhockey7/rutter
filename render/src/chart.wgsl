// Chart rendering.
//
// Vertices arrive as an offset from a *reference point* in normalised world
// units — the 0..1 Web Mercator square from rutter-core's chart module. That
// choice is the whole point of this shader: world coordinates near the poles
// or at zoom 20 need more precision than f32 has, but an offset from a
// nearby reference is always a small number, so f32 is plenty and the
// arithmetic stays on the GPU.
//
// Each vertex buffer carries its own reference point, and `origin` is that
// point's position relative to the centre of the screen. Two consequences
// make the whole design work:
//
//   * The chart's reference point is fixed when its geometry is uploaded, so
//     panning changes only `origin` — four bytes in a uniform — and never
//     the vertex buffer. Storing chart vertices relative to the view centre
//     instead would mean re-uploading every coastline on every frame of a
//     drag.
//
//   * The tile grid is regenerated per frame anyway, so its reference point
//     *is* the view centre and its `origin` is zero.
//
// The CPU therefore never computes clip space. It subtracts in f64 and hands
// over two small floats.

// ── The under-keel check, on the GPU ───────────────────────────────────
//
// Whether a charted depth is passable depends on the tide, and the tide
// changes — every second in principle, and a whole day at a time once
// there is a time slider to drag. The first version of this baked the
// verdict into vertex *colours* on the CPU, which meant that moving the
// tide re-decoded and re-uploaded the chart: 413,000 vertices in a
// harbour view, for an answer that is two comparisons.
//
// So the vertex carries the charted depth instead of a verdict, and the
// verdict is computed here from a uniform. Changing the tide is now a
// 32-byte write, and a dragged slider costs nothing.
//
// The rule is `rutter_core::depth::assess`, which is three lines, and it
// is mirrored below. Two implementations of a navigation rule is a real
// risk, so `clearance_matches_the_core_rule` in lib.rs walks a grid of
// depths and tides through both and fails if they ever disagree, and
// `the_shader_palette_matches_the_rust_one` parses the constants out of
// this file. Neither can drift silently.

struct ViewUniform {
	// Clip units per world unit, x and y. They differ because the viewport
	// is rarely square.
	scale: vec2<f32>,
	// This buffer's reference point, relative to the view centre, in world
	// units.
	origin: vec2<f32>,
	// The under-keel check, as four floats so the struct stays one neat
	// 32-byte block:
	//   x  1 when the check is on, 0 when off
	//   y  draft plus the margin wanted under it, metres
	//   z  tide height now, metres above chart datum
	//   w  the lowest tide in the window, metres above chart datum
	check: vec4<f32>,
};

@group(0) @binding(0) var<uniform> view: ViewUniform;

// What a vertex puts in `depth` when it is not a depth at all: a light's
// height above datum, a buoy, a coastline, the tile grid. Anything at or
// below this is "do not judge me".
//
// A sentinel rather than NaN because WGSL makes no promises about NaN
// comparisons, and a chart that silently stopped judging would be the
// worst possible way for that to show up.
const NO_DEPTH: f32 = -1.0e8;

// Which way a tie falls.
//
// The core rule computes in f64; this computes in f32, and the two can
// disagree by one unit in the last place. At a charted -0.3 m with 2.3 m of
// tide, f64 gives exactly 2.0 and f32 gives 1.99999988 — so a boat needing
// 2.0 m is either clear or foul depending on which arithmetic answered.
//
// A millimetre, added to what the boat needs, decides it the only way a
// depth check may be decided: against the boat. The shader is now never
// more optimistic than the core rule, and a millimetre is four orders of
// magnitude below the decimetre a chart states soundings to, so nothing
// real is affected. `clearance_matches_the_core_rule` asserts both halves
// of that — exact agreement away from the boundary, and never less severe
// at it.
const TIE_MARGIN: f32 = 1.0e-3;

// Verdicts, matching rutter_core::depth::Clearance.
const CLEAR: i32 = 0;
const TIGHT: i32 = 1;
const FOUL: i32 = 2;
const UNJUDGED: i32 = 3;

// Linework and marks, from `colour_for_clearance`.
const CLEAR_LINE = vec4<f32>(0.35, 0.85, 0.45, 1.0);
const TIGHT_LINE = vec4<f32>(1.0, 0.72, 0.15, 1.0);
const FOUL_LINE = vec4<f32>(1.0, 0.25, 0.25, 1.0);

// Area fills, from `clearance_fill_for`. Lighter, because the imagery and
// the linework have to stay readable through them.
const TIGHT_FILL = vec4<f32>(1.0, 0.72, 0.15, 0.40);
const FOUL_FILL = vec4<f32>(1.0, 0.25, 0.25, 0.45);
// Clear water is not shaded at all: with the check on, the question is
// "where can I not go", and flooding the safe water would bury the answer.
const CLEAR_FILL = vec4<f32>(0.0, 0.0, 0.0, 0.0);

// `rutter_core::depth::assess`, mirrored.
//
// `depth` is metres below chart datum as the chart prints it, so a drying
// height is negative — which this handles without a special case, exactly
// as the Rust does, and for the same reason: the sign convention is the
// chart's rather than one invented here.
fn verdict_of(depth: f32) -> i32 {
	if view.check.x == 0.0 || depth <= NO_DEPTH {
		return UNJUDGED;
	}
	let needed = view.check.y + TIE_MARGIN;
	if depth + view.check.z < needed {
		return FOUL;
	}
	if depth + view.check.w < needed {
		return TIGHT;
	}
	return CLEAR;
}

// How much bigger to draw a mark that is a warning.
//
// A cross is built with its full-size arm in `arm`, and scaled here. Doing
// it on the GPU is what lets the verdict change without rebuilding the
// geometry — and the crosses are the one thing whose *size* carries
// meaning, so they could not simply be recoloured.
fn emphasis_of(verdict: i32) -> f32 {
	if verdict == FOUL {
		return 1.5;
	}
	if verdict == TIGHT {
		return 1.2;
	}
	return 1.0;
}

struct VertexOut {
	@builtin(position) clip: vec4<f32>,
	@location(0) colour: vec4<f32>,
};

fn place(offset: vec2<f32>, arm: vec2<f32>, emphasis: f32) -> vec4<f32> {
	let world = offset + arm * emphasis + view.origin;

	// World y grows southward, clip y grows northward: the negation is the
	// only place that flip happens.
	return vec4<f32>(
		world.x * view.scale.x,
		-world.y * view.scale.y,
		0.0,
		1.0,
	);
}

// Linework, marks and the tile grid.
@vertex
fn vs_main(
	@location(0) offset: vec2<f32>,
	@location(1) arm: vec2<f32>,
	@location(2) colour: vec4<f32>,
	@location(3) depth: f32,
) -> VertexOut {
	var out: VertexOut;
	let verdict = verdict_of(depth);

	out.clip = place(offset, arm, emphasis_of(verdict));
	// With the check on, a judged feature is coloured by its verdict and
	// nothing else — a sounding the boat cannot pass should look like a
	// warning, not like a sounding drawn slightly differently.
	switch verdict {
		case 0: { out.colour = CLEAR_LINE; }
		case 1: { out.colour = TIGHT_LINE; }
		case 2: { out.colour = FOUL_LINE; }
		default: { out.colour = colour; }
	}
	return out;
}

// Area fills. The same vertex data; only the Clear case differs, because
// an unshaded fill has to fall back to its depth band rather than to a
// green wash.
@vertex
fn vs_fill(
	@location(0) offset: vec2<f32>,
	@location(1) arm: vec2<f32>,
	@location(2) colour: vec4<f32>,
	@location(3) depth: f32,
) -> VertexOut {
	var out: VertexOut;
	let verdict = verdict_of(depth);

	// No emphasis for a fill: it has an extent of its own, and scaling a
	// polygon about its vertices' reference point would tear it.
	out.clip = place(offset, arm, 1.0);
	switch verdict {
		case 0: { out.colour = CLEAR_FILL; }
		case 1: { out.colour = TIGHT_FILL; }
		case 2: { out.colour = FOUL_FILL; }
		default: { out.colour = colour; }
	}
	return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
	return in.colour;
}
