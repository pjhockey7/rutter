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

struct ViewUniform {
	// Clip units per world unit, x and y. They differ because the viewport
	// is rarely square.
	scale: vec2<f32>,
	// This buffer's reference point, relative to the view centre, in world
	// units.
	origin: vec2<f32>,
};

@group(0) @binding(0) var<uniform> view: ViewUniform;

struct VertexOut {
	@builtin(position) clip: vec4<f32>,
	@location(0) colour: vec4<f32>,
};

@vertex
fn vs_main(
	@location(0) offset: vec2<f32>,
	@location(1) colour: vec4<f32>,
) -> VertexOut {
	var out: VertexOut;

	let world = offset + view.origin;

	// World y grows southward, clip y grows northward: the negation is the
	// only place that flip happens.
	out.clip = vec4<f32>(
		world.x * view.scale.x,
		-world.y * view.scale.y,
		0.0,
		1.0,
	);
	out.colour = colour;
	return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
	return in.colour;
}
