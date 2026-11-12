// Overlay marks: anchored to the chart, sized on the screen.
//
// Everything else this renderer draws is geometry in world units, which
// means it grows and shrinks with the zoom — which is correct for a
// coastline and wrong for a symbol. A tide station's ring and a wind barb
// are *notation*: they mark a place without claiming an extent, so they
// should stay the same size however far in you are, exactly as a buoy
// symbol does on a paper chart.
//
// So each vertex carries two offsets:
//
//   * `anchor` — where the mark belongs, in world units relative to this
//     buffer's reference point, the same scheme as chart.wgsl. This is what
//     makes the mark pan and zoom with the chart.
//   * `pixel` — where this vertex sits relative to the anchor, in screen
//     pixels. This is what makes the size independent of zoom.
//
// The alternative was to draw these through the interface layer, which is
// how they started: egui gives constant screen size for nothing, because it
// works in points already. It also puts chart notation in a different
// renderer with a different palette and a different idea of anti-aliasing,
// and re-tessellates it every frame. A mark on the chart belongs to the
// chart.

struct OverlayUniform {
	// Clip units per world unit, x and y.
	scale: vec2<f32>,
	// This buffer's reference point relative to the view centre.
	origin: vec2<f32>,
	// Viewport size in pixels, to turn a pixel offset into clip units.
	viewport: vec2<f32>,
	// WGSL rounds a uniform struct up to 16 bytes; spelled out so the Rust
	// and WGSL layouts are obviously the same rather than accidentally so.
	// Two vec2s and one more is 24, rounded to 32.
	_pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> view: OverlayUniform;

struct VertexOut {
	@builtin(position) clip: vec4<f32>,
	@location(0) colour: vec4<f32>,
};

@vertex
fn vs_main(
	@location(0) anchor: vec2<f32>,
	@location(1) pixel: vec2<f32>,
	@location(2) colour: vec4<f32>,
) -> VertexOut {
	var out: VertexOut;

	let world = anchor + view.origin;

	// World y grows southward and clip y northward, so one flip — the same
	// single negation chart.wgsl does. The pixel offset is already in screen
	// terms, where y also grows downward, so it takes the same flip.
	let from_world = vec2<f32>(world.x * view.scale.x, -world.y * view.scale.y);

	// Clip space spans -1..1 across the viewport, so one pixel is 2/width.
	let from_pixels = vec2<f32>(
		pixel.x * 2.0 / max(view.viewport.x, 1.0),
		-pixel.y * 2.0 / max(view.viewport.y, 1.0),
	);

	out.clip = vec4<f32>(from_world + from_pixels, 0.0, 1.0);
	out.colour = colour;
	return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
	return in.colour;
}
