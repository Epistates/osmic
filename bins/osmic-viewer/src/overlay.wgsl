// The CPU-rasterised label overlay: a premultiplied RGBA image drawn over
// the map. Between re-layouts it is moved (by whole pixels, drawn 1:1) or,
// while zooming, scaled with the map.

struct Overlay {
    rect: vec4<f32>,      // x, y, width, height on screen in physical px
    viewport: vec2<f32>,  // physical px
    linearize: f32,
    pad: f32,
};

@group(0) @binding(0) var<uniform> ov: Overlay;
@group(0) @binding(1) var overlay_texture: texture_2d<f32>;
@group(0) @binding(2) var overlay_sampler: sampler;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32(i & 1u), f32((i >> 1u) & 1u));
    let px = ov.rect.xy + corner * ov.rect.zw;
    return vec4<f32>(px.x / ov.viewport.x * 2.0 - 1.0, 1.0 - px.y / ov.viewport.y * 2.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(overlay_texture));
    let local = frag.xy - ov.rect.xy;
    if (any(local < vec2<f32>(0.0)) || any(local >= ov.rect.zw)) {
        return vec4<f32>(0.0);
    }
    var c: vec4<f32>;
    if (all(abs(ov.rect.zw - dims) < vec2<f32>(0.5))) {
        // Drawn 1:1: copy texels exactly.
        c = textureLoad(overlay_texture, vec2<i32>(floor(local)), 0);
    } else {
        c = textureSampleLevel(overlay_texture, overlay_sampler, local / ov.rect.zw, 0.0);
    }
    if (ov.linearize > 0.5 && c.a > 0.0) {
        c = vec4<f32>(srgb_to_linear(c.rgb / c.a) * c.a, c.a);
    }
    return c;
}
