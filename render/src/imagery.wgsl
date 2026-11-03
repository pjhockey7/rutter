// Aerial imagery: one textured quad per tile.
//
// The same offset-from-a-reference-point scheme as the chart lines, and for
// the same reason: absolute Web Mercator coordinates exceed f32 at high
// zoom. See chart.wgsl for the full argument. `origin` is the imagery
// layer's reference point relative to the view centre, so panning updates
// one uniform and leaves the quads and their textures untouched.
//
// Drawn before everything else, so the chart reads on top of the photograph
// rather than under it.

struct ViewUniform {
	// Clip units per world unit, x and y.
	scale: vec2<f32>,
	// This layer's reference point relative to the view centre.
	origin: vec2<f32>,
	// How strongly the imagery shows through. Chart linework has to stay
	// legible over a bright aerial photograph, and fading the photograph is
	// the cheapest way to keep contrast without restyling every line.
	opacity: f32,

	// No explicit padding member here, deliberately.
	//
	// A uniform struct is rounded up to a multiple of 16 bytes, so this is
	// 8 + 8 + 4 = 20 rounded to 32 — which matches the Rust struct's
	// trailing [f32; 3]. Writing the padding out as `_pad: vec3<f32>` does
	// NOT match: vec3 carries 16-byte alignment, which pushes it to offset
	// 32 and the struct to 48. wgpu catches the disagreement, but not until
	// the first draw: "bound with size 32 where the shader expects 48".
};

@group(0) @binding(0) var<uniform> view: ViewUniform;
@group(1) @binding(0) var tile: texture_2d<f32>;
@group(1) @binding(1) var tile_sampler: sampler;

struct VertexOut {
	@builtin(position) clip: vec4<f32>,
	@location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(
	@location(0) offset: vec2<f32>,
	@location(1) uv: vec2<f32>,
) -> VertexOut {
	var out: VertexOut;

	let world = offset + view.origin;

	// World y grows southward, clip y northward — the same single flip the
	// chart shader does.
	out.clip = vec4<f32>(
		world.x * view.scale.x,
		-world.y * view.scale.y,
		0.0,
		1.0,
	);
	out.uv = uv;
	return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
	let texel = textureSample(tile, tile_sampler, in.uv);
	// Premultiplied against the sea colour behind it by the blend state, so
	// opacity simply scales alpha.
	return vec4<f32>(texel.rgb, texel.a * view.opacity);
}
