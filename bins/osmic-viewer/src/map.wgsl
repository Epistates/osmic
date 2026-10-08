// Tile geometry. Positions are tile-local pixels; strokes and circles are
// extruded here, in screen space, so their width in pixels is independent
// of the camera zoom.

struct Draw {
    offset: vec2<f32>,    // screen position (logical px) of the tile origin
    viewport: vec2<f32>,  // logical px
    scale: f32,           // screen px per tile px
    tile_zoom: f32,
    camera_zoom: f32,
    linearize: f32,       // 1.0 on sRGB surfaces
};

@group(0) @binding(0) var<uniform> draw: Draw;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) extrude: vec2<f32>,
    @location(2) half_width: vec2<f32>,  // at tile zoom, at tile zoom + 1
    @location(3) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

@vertex
fn vs_main(v: VertexInput) -> VertexOutput {
    // Blend the style's width at this zoom with its width at the next one
    // as the camera moves between them.
    let t = clamp(draw.camera_zoom - draw.tile_zoom, 0.0, 1.0);
    let half_width = mix(v.half_width.x, v.half_width.y, t);
    let screen = draw.offset + v.position * draw.scale + v.extrude * half_width;

    var out: VertexOutput;
    out.clip = vec4<f32>(
        screen.x / draw.viewport.x * 2.0 - 1.0,
        1.0 - screen.y / draw.viewport.y * 2.0,
        0.0,
        1.0,
    );
    var rgb = v.color.rgb;
    if (draw.linearize > 0.5) {
        rgb = srgb_to_linear(rgb);
    }
    out.color = vec4<f32>(rgb, v.color.a);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
