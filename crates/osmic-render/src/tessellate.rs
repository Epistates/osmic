//! Lyon tessellation of a [`SceneGraph`] into GPU-ready triangle meshes.
//!
//! The mesh is designed for a vertex shader that keeps strokes and circles
//! a constant number of **screen** pixels wide at any zoom:
//!
//! ```text
//! screen = project(position) + extrude * mix(half_width[0], half_width[1], t)
//! ```
//!
//! where `position` is in scene pixels (project it with the tile's
//! transform), `extrude` is a unit-scale offset direction produced from
//! lyon's stroke normals (miters and round joins included), and `t` is the
//! fractional zoom between the mesh's zoom and the next. Fills have
//! `extrude == 0`. Because widths are extruded in screen space the shader
//! never has to re-tessellate when the camera zooms.

use bytemuck::{Pod, Zeroable};
use lyon::math::point;
use lyon::path::Path;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, LineCap as LyonCap,
    LineJoin as LyonJoin, StrokeOptions, StrokeTessellator, StrokeVertex, VertexBuffers,
};
use osmic_core::Color;

use crate::scene::{LineCap, LineJoin, RenderFeature, SceneGraph};

/// One mesh vertex (28 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct MeshVertex {
    /// Position in scene pixels, before extrusion.
    pub position: [f32; 2],
    /// Direction (and miter scale) in which to push the vertex outward, in
    /// units of the half width.
    pub extrude: [f32; 2],
    /// Half the line width (or the circle radius) in screen pixels, at this
    /// mesh's zoom and at the next integer zoom.
    pub half_width: [f32; 2],
    /// Straight-alpha RGBA8, in the color space the style was written in
    /// (sRGB for CSS colors).
    pub color: [u8; 4],
}

/// Triangle list mesh.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<MeshVertex>,
    pub indices: Vec<u32>,
    /// Primitives that could not be tessellated (degenerate geometry).
    pub skipped: usize,
    /// Whether the vertex budget ran out and later primitives were dropped.
    pub truncated: bool,
}

/// Limits for [`tessellate_scene`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TessellationOptions {
    /// Stop adding primitives once the mesh has this many vertices.
    pub max_vertices: usize,
}

impl Default for TessellationOptions {
    fn default() -> Self {
        Self {
            max_vertices: 4_000_000,
        }
    }
}

/// Curve flattening tolerance, in scene pixels.
const TOLERANCE: f32 = 0.1;

fn rgba(c: Color) -> [u8; 4] {
    c.to_rgba8()
}

/// Tessellate everything except labels, in draw order (layers by
/// `z_order`, then feature order), so painting the mesh back to front with
/// alpha blending reproduces the scene.
pub fn tessellate_scene(scene: &SceneGraph, options: &TessellationOptions) -> Mesh {
    let mut buffers: VertexBuffers<MeshVertex, u32> = VertexBuffers::new();
    let mut fill = FillTessellator::new();
    let mut stroke = StrokeTessellator::new();
    let (mut skipped, mut truncated) = (0usize, false);

    let mut layers: Vec<_> = scene.layers.iter().collect();
    layers.sort_by_key(|l| l.z_order);
    'outer: for layer in layers {
        for feature in &layer.features {
            if buffers.vertices.len() >= options.max_vertices {
                truncated = true;
                break 'outer;
            }
            let ok = match feature {
                RenderFeature::Fill { coords, color } => {
                    tessellate_fill(&mut fill, &mut buffers, coords, rgba(*color))
                }
                RenderFeature::Stroke {
                    coords,
                    color,
                    width,
                    width_next_zoom,
                    cap,
                    join,
                    dash,
                } => tessellate_stroke(
                    &mut stroke,
                    &mut buffers,
                    coords,
                    StrokeSpec {
                        color: rgba(*color),
                        width: [*width, *width_next_zoom],
                        cap: *cap,
                        join: *join,
                        dash,
                    },
                ),
                RenderFeature::Circle {
                    center,
                    radius,
                    radius_next_zoom,
                    color,
                    stroke_color,
                    stroke_width,
                } => {
                    if *stroke_width > 0.0 && stroke_color.a > 0.0 {
                        disc(
                            &mut buffers,
                            *center,
                            [*radius + stroke_width, *radius_next_zoom + stroke_width],
                            rgba(*stroke_color),
                        );
                    }
                    if color.a > 0.0 {
                        disc(
                            &mut buffers,
                            *center,
                            [*radius, *radius_next_zoom],
                            rgba(*color),
                        );
                    }
                    true
                }
                RenderFeature::Label(_) => true,
            };
            if !ok {
                skipped += 1;
            }
        }
    }
    Mesh {
        vertices: buffers.vertices,
        indices: buffers.indices,
        skipped,
        truncated,
    }
}

fn tessellate_fill(
    tess: &mut FillTessellator,
    out: &mut VertexBuffers<MeshVertex, u32>,
    rings: &[Vec<[f32; 2]>],
    color: [u8; 4],
) -> bool {
    let mut builder = Path::builder();
    let mut any = false;
    for ring in rings.iter().filter(|r| r.len() >= 3) {
        builder.begin(point(ring[0][0], ring[0][1]));
        for p in &ring[1..] {
            builder.line_to(point(p[0], p[1]));
        }
        builder.end(true);
        any = true;
    }
    if !any {
        return false;
    }
    let path = builder.build();
    tess.tessellate_path(
        &path,
        &FillOptions::tolerance(TOLERANCE).with_fill_rule(FillRule::EvenOdd),
        &mut BuffersBuilder::new(out, |v: FillVertex<'_>| MeshVertex {
            position: v.position().to_array(),
            extrude: [0.0, 0.0],
            half_width: [0.0, 0.0],
            color,
        }),
    )
    .is_ok()
}

struct StrokeSpec<'a> {
    color: [u8; 4],
    /// Full width at this zoom and the next.
    width: [f32; 2],
    cap: LineCap,
    join: LineJoin,
    dash: &'a [f32],
}

fn tessellate_stroke(
    tess: &mut StrokeTessellator,
    out: &mut VertexBuffers<MeshVertex, u32>,
    coords: &[[f32; 2]],
    spec: StrokeSpec<'_>,
) -> bool {
    if coords.len() < 2 || spec.width[0].max(spec.width[1]) <= 0.0 {
        return false;
    }
    let closed = coords.len() > 3 && coords.first() == coords.last();
    let mut builder = Path::builder();
    if spec.dash.is_empty() {
        let pts = if closed {
            &coords[..coords.len() - 1]
        } else {
            coords
        };
        builder.begin(point(pts[0][0], pts[0][1]));
        for p in &pts[1..] {
            builder.line_to(point(p[0], p[1]));
        }
        builder.end(closed);
    } else {
        for piece in dash_polyline(coords, spec.dash) {
            builder.begin(point(piece[0][0], piece[0][1]));
            for p in &piece[1..] {
                builder.line_to(point(p[0], p[1]));
            }
            builder.end(false);
        }
    }
    let path = builder.build();

    // Tessellate at the wider of the two widths so round joins and caps
    // have enough segments; vertices carry only direction, and the shader
    // supplies the real half width.
    let nominal = spec.width[0].max(spec.width[1]);
    let half_widths = [spec.width[0] / 2.0, spec.width[1] / 2.0];
    let cap = match spec.cap {
        LineCap::Butt => LyonCap::Butt,
        LineCap::Round => LyonCap::Round,
        LineCap::Square => LyonCap::Square,
    };
    let join = match spec.join {
        LineJoin::Miter => LyonJoin::Miter,
        LineJoin::Round => LyonJoin::Round,
        LineJoin::Bevel => LyonJoin::Bevel,
    };
    let color = spec.color;
    tess.tessellate_path(
        &path,
        &StrokeOptions::tolerance(TOLERANCE)
            .with_line_width(nominal)
            .with_line_cap(cap)
            .with_line_join(join)
            .with_miter_limit(2.0),
        &mut BuffersBuilder::new(out, |v: StrokeVertex<'_, '_>| MeshVertex {
            position: v.position_on_path().to_array(),
            extrude: v.normal().to_array(),
            half_width: half_widths,
            color,
        }),
    )
    .is_ok()
}

/// A triangle-fan disc of radius `half_width` (screen px) around `center`.
fn disc(
    out: &mut VertexBuffers<MeshVertex, u32>,
    center: [f32; 2],
    half_width: [f32; 2],
    color: [u8; 4],
) {
    let radius = half_width[0].max(half_width[1]);
    let segments = ((radius * 2.0).ceil() as u32).clamp(10, 40);
    let base = out.vertices.len() as u32;
    out.vertices.push(MeshVertex {
        position: center,
        extrude: [0.0, 0.0],
        half_width: [0.0, 0.0],
        color,
    });
    for i in 0..segments {
        let theta = i as f32 / segments as f32 * std::f32::consts::TAU;
        out.vertices.push(MeshVertex {
            position: center,
            extrude: [theta.cos(), theta.sin()],
            half_width,
            color,
        });
    }
    for i in 0..segments {
        out.indices
            .extend([base, base + 1 + i, base + 1 + (i + 1) % segments]);
    }
}

/// Split a polyline into the "on" pieces of a dash pattern (alternating
/// on/off lengths, repeating).
pub fn dash_polyline(line: &[[f32; 2]], pattern: &[f32]) -> Vec<Vec<[f32; 2]>> {
    let total: f32 = pattern.iter().sum();
    if line.len() < 2
        || pattern.is_empty()
        || !pattern.len().is_multiple_of(2)
        || total.is_nan()
        || total <= 0.0
    {
        return vec![line.to_vec()];
    }
    let mut pieces: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();
    let (mut index, mut remaining) = (0usize, pattern[0]);
    let mut on = true;
    if on {
        current.push(line[0]);
    }
    for w in line.windows(2) {
        let (a, b) = (w[0], w[1]);
        let seg = (b[0] - a[0]).hypot(b[1] - a[1]);
        if seg <= 0.0 {
            continue;
        }
        let mut travelled = 0.0;
        while seg - travelled > remaining {
            travelled += remaining;
            let t = travelled / seg;
            let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            if on {
                current.push(p);
                pieces.push(std::mem::take(&mut current));
            } else {
                current.push(p);
            }
            on = !on;
            index = (index + 1) % pattern.len();
            remaining = pattern[index];
        }
        remaining -= seg - travelled;
        if on {
            current.push(b);
        }
    }
    if on && current.len() >= 2 {
        pieces.push(current);
    }
    pieces.retain(|p| p.len() >= 2);
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::RenderLayer;

    fn scene_of(features: Vec<RenderFeature>) -> SceneGraph {
        let mut layer = RenderLayer::new(0);
        layer.features = features;
        let mut scene = SceneGraph::new(Color::WHITE);
        scene.add_layer(layer);
        scene
    }

    fn stroke(coords: Vec<[f32; 2]>, width: f32, next: f32) -> RenderFeature {
        RenderFeature::Stroke {
            coords,
            color: Color::rgb(1.0, 0.0, 0.0),
            width,
            width_next_zoom: next,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            dash: vec![],
        }
    }

    fn triangle_area(m: &Mesh, i: usize) -> f32 {
        let p = |k: usize| m.vertices[m.indices[i * 3 + k] as usize].position;
        let (a, b, c) = (p(0), p(1), p(2));
        ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])).abs() / 2.0
    }

    #[test]
    fn fill_triangles_cover_the_polygon_area() {
        let square = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let mesh = tessellate_scene(
            &scene_of(vec![RenderFeature::Fill {
                coords: vec![square],
                color: Color::BLACK,
            }]),
            &TessellationOptions::default(),
        );
        let area: f32 = (0..mesh.indices.len() / 3)
            .map(|i| triangle_area(&mesh, i))
            .sum();
        assert!((area - 100.0).abs() < 1e-3, "{area}");
        assert!(
            mesh.vertices
                .iter()
                .all(|v| v.extrude == [0.0, 0.0] && v.half_width == [0.0, 0.0])
        );
    }

    #[test]
    fn fill_holes_are_cut_out() {
        let outer = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let hole = vec![[3.0, 3.0], [7.0, 3.0], [7.0, 7.0], [3.0, 7.0]];
        let mesh = tessellate_scene(
            &scene_of(vec![RenderFeature::Fill {
                coords: vec![outer, hole],
                color: Color::BLACK,
            }]),
            &TessellationOptions::default(),
        );
        let area: f32 = (0..mesh.indices.len() / 3)
            .map(|i| triangle_area(&mesh, i))
            .sum();
        assert!((area - 84.0).abs() < 1e-3, "{area}");
    }

    #[test]
    fn stroke_vertices_extrude_to_the_requested_width() {
        let mesh = tessellate_scene(
            &scene_of(vec![stroke(vec![[0.0, 0.0], [100.0, 0.0]], 4.0, 8.0)]),
            &TessellationOptions::default(),
        );
        assert!(!mesh.vertices.is_empty());
        for v in &mesh.vertices {
            assert_eq!(v.half_width, [2.0, 4.0]);
            // A butt-capped straight line extrudes exactly perpendicular.
            assert!(
                (v.extrude[0]).abs() < 1e-5 && (v.extrude[1].abs() - 1.0).abs() < 1e-5,
                "{v:?}"
            );
        }
        // Applying the first half width reproduces the stroke rectangle.
        let ys: Vec<f32> = mesh
            .vertices
            .iter()
            .map(|v| v.position[1] + v.extrude[1] * v.half_width[0])
            .collect();
        assert!(ys.iter().all(|y| (y.abs() - 2.0).abs() < 1e-4));
    }

    #[test]
    fn miter_joins_scale_the_extrusion() {
        let mesh = tessellate_scene(
            &scene_of(vec![stroke(
                vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]],
                2.0,
                2.0,
            )]),
            &TessellationOptions::default(),
        );
        let longest = mesh
            .vertices
            .iter()
            .map(|v| v.extrude[0].hypot(v.extrude[1]))
            .fold(0.0, f32::max);
        assert!(
            (longest - std::f32::consts::SQRT_2).abs() < 1e-3,
            "{longest}"
        );
    }

    #[test]
    fn closed_rings_have_no_caps() {
        let ring = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ];
        let mesh = tessellate_scene(
            &scene_of(vec![stroke(ring, 2.0, 2.0)]),
            &TessellationOptions::default(),
        );
        let area: f32 = (0..mesh.indices.len() / 3)
            .map(|i| {
                let p = |k: usize| {
                    let v = mesh.vertices[mesh.indices[i * 3 + k] as usize];
                    [
                        v.position[0] + v.extrude[0] * v.half_width[0],
                        v.position[1] + v.extrude[1] * v.half_width[0],
                    ]
                };
                let (a, b, c) = (p(0), p(1), p(2));
                ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])).abs() / 2.0
            })
            .sum();
        // Outer 12x12 minus inner 8x8.
        assert!((area - 80.0).abs() < 0.5, "{area}");
    }

    #[test]
    fn dashes_split_the_line() {
        let pieces = dash_polyline(&[[0.0, 0.0], [100.0, 0.0]], &[10.0, 10.0]);
        assert_eq!(pieces.len(), 5);
        for (i, p) in pieces.iter().enumerate() {
            assert!((p[0][0] - 20.0 * i as f32).abs() < 1e-4);
            assert!((p.last().unwrap()[0] - (20.0 * i as f32 + 10.0)).abs() < 1e-4);
        }
        // Pattern state carries across vertices.
        let bent = dash_polyline(&[[0.0, 0.0], [15.0, 0.0], [15.0, 25.0]], &[10.0, 10.0]);
        let total: f32 = bent
            .iter()
            .map(|p| {
                p.windows(2)
                    .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
                    .sum::<f32>()
            })
            .sum();
        assert!((total - 20.0).abs() < 1e-3, "{total}");
        assert_eq!(dash_polyline(&[[0.0, 0.0], [5.0, 0.0]], &[]).len(), 1);
    }

    #[test]
    fn circles_are_extruded_discs_with_an_optional_outline() {
        let circle = |stroke_width| RenderFeature::Circle {
            center: [5.0, 5.0],
            radius: 3.0,
            radius_next_zoom: 4.0,
            color: Color::BLACK,
            stroke_color: Color::WHITE,
            stroke_width,
        };
        let plain = tessellate_scene(
            &scene_of(vec![circle(0.0)]),
            &TessellationOptions::default(),
        );
        let outlined = tessellate_scene(
            &scene_of(vec![circle(1.0)]),
            &TessellationOptions::default(),
        );
        assert_eq!(outlined.vertices.len(), plain.vertices.len() * 2);
        assert!(plain.vertices.iter().all(|v| v.position == [5.0, 5.0]));
        let rim = plain
            .vertices
            .iter()
            .find(|v| v.extrude != [0.0, 0.0])
            .unwrap();
        assert_eq!(rim.half_width, [3.0, 4.0]);
        // The outline is drawn first and is larger.
        assert_eq!(outlined.vertices[1].half_width, [4.0, 5.0]);
    }

    #[test]
    fn draw_order_follows_z_order_and_labels_are_ignored() {
        let mut low = RenderLayer::new(1);
        low.push(RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]],
            color: Color::rgb(0.0, 1.0, 0.0),
        });
        let mut high = RenderLayer::new(5);
        high.push(RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]],
            color: Color::rgb(1.0, 0.0, 0.0),
        });
        let mut scene = SceneGraph::new(Color::WHITE);
        scene.add_layer(high);
        scene.add_layer(low);
        let mesh = tessellate_scene(&scene, &TessellationOptions::default());
        assert_eq!(mesh.vertices[0].color, [0, 255, 0, 255]);
        assert_eq!(mesh.vertices.last().unwrap().color, [255, 0, 0, 255]);
    }

    #[test]
    fn degenerate_geometry_is_skipped_and_counted() {
        let mesh = tessellate_scene(
            &scene_of(vec![
                RenderFeature::Fill {
                    coords: vec![vec![[0.0, 0.0], [1.0, 1.0]]],
                    color: Color::BLACK,
                },
                stroke(vec![[0.0, 0.0]], 2.0, 2.0),
                stroke(vec![[0.0, 0.0], [5.0, 0.0]], 0.0, 0.0),
            ]),
            &TessellationOptions::default(),
        );
        assert_eq!(mesh.skipped, 3);
        assert!(mesh.vertices.is_empty());
    }

    #[test]
    fn vertex_budget_truncates() {
        let many: Vec<RenderFeature> = (0..100)
            .map(|i| stroke(vec![[0.0, i as f32], [10.0, i as f32]], 2.0, 2.0))
            .collect();
        let mesh = tessellate_scene(&scene_of(many), &TessellationOptions { max_vertices: 50 });
        assert!(mesh.truncated);
        assert!(mesh.vertices.len() < 100 * 4);
        assert!(
            mesh.indices
                .iter()
                .all(|&i| (i as usize) < mesh.vertices.len())
        );
    }

    #[test]
    fn vertex_layout_is_28_bytes() {
        assert_eq!(std::mem::size_of::<MeshVertex>(), 28);
    }
}
