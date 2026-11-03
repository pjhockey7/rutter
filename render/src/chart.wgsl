// Chart rendering.
//
// Vertices arrive as an offset from the view centre in normalised world
// units — the 0..1 Web Mercator square from rutter-core's chart module. That
// choice is the whole point of this shader: world coordinates near the poles
// or at zoom 20 need more than f32 has, but an *offset from the centre of
// the screen* is always a small number, so f32 is plenty and the arithmetic
// stays on the GPU.
//
// The CPU therefore never computes clip space. It subtracts the centre and
// hands over the scale.

struct ViewUniform {
	// Clip units per world unit, x and y. Differ because the viewport is
	// rarely square.
	scale: vec2<f32>,
	// Pixel size of one world unit, for line widths that do not change with
	// zoom.
	pixels_per_world: f32,
	_pad: f32,
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
	// World y grows southward, clip y grows northward: the negation is the
	// only place that flip happens.
	out.clip = vec4<f32>(
		offset.x * view.scale.x,
		-offset.y * view.scale.y,
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
