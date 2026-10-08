use tiny_skia::{
    Color as SkiaColor, FillRule, IntRect, LineCap as SkiaCap, LineJoin as SkiaJoin, Paint, Path,
    PathBuilder, Pixmap, PixmapMut, Stroke, StrokeDash, Transform,
};
use tracing::info;

use osmic_core::Color;
use osmic_text::{Canvas, LabelCandidate, LabelPlacer, Rect, TextEngine, unpremultiply};

use crate::backend::{RenderBackend, RenderConfig};
use crate::error::{RenderError, RenderResult};
use crate::scene::{LineCap, LineJoin, RenderFeature, RenderLayer, SceneGraph};
use crate::tessellate::{MITER_LIMIT, dash_is_drawable, polyline_length};

/// Software rendering backend: tiny-skia for geometry, [`osmic_text`] for
/// labels.
///
/// The pixmap is premultiplied RGBA8 internally; [`RenderBackend::read_pixels`]
/// converts to straight alpha.
pub struct SkiaBackend {
    pixmap: Pixmap,
    config: RenderConfig,
    text: TextEngine,
}

fn validate_ratio(ratio: f32) -> RenderResult<()> {
    if ratio.is_finite() && ratio > 0.0 {
        Ok(())
    } else {
        Err(RenderError::InvalidPixelRatio(ratio))
    }
}

/// Physical size of a logical dimension: rounded, never zero.
fn scaled_dim(logical: u32, ratio: f32) -> u32 {
    ((f64::from(logical) * f64::from(ratio)).round() as u32).max(1)
}

impl RenderBackend for SkiaBackend {
    /// Uses the system fonts for text; see [`SkiaBackend::with_text_engine`]
    /// to supply fonts explicitly.
    fn init(config: &RenderConfig) -> RenderResult<Self> {
        Self::with_text_engine(config, TextEngine::system())
    }

    fn render(&mut self, scene: &SceneGraph) -> RenderResult<()> {
        self.pixmap.fill(to_skia_color(&scene.background)?);

        let ratio = self.config.pixel_ratio;
        let transform = Transform::from_scale(ratio, ratio);

        let mut layers: Vec<&RenderLayer> = scene.layers.iter().collect();
        layers.sort_by_key(|l| l.z_order); // stable

        // Labels are placed across all layers at once so priorities apply,
        // then drawn on top of the geometry. They are never clipped.
        let labels: Vec<&LabelCandidate> = layers
            .iter()
            .flat_map(|l| &l.features)
            .filter_map(|f| match f {
                RenderFeature::Label(candidate) => Some(candidate),
                _ => None,
            })
            .collect();

        let mut rest = layers.as_slice();
        while let Some(first) = rest.first() {
            let clipped = first.clip.is_some();
            let run = rest
                .iter()
                .take_while(|l| l.clip.is_some() == clipped)
                .count();
            let (now, later) = rest.split_at(run);
            if clipped {
                self.draw_clipped(now, transform)?;
            } else {
                for layer in now {
                    draw_features(&mut self.pixmap.as_mut(), &layer.features, transform)?;
                }
            }
            rest = later;
        }

        self.render_labels(&labels, ratio, [0.0, 0.0]);
        Ok(())
    }

    fn read_pixels(&self) -> Option<Vec<u8>> {
        Some(unpremultiply(self.pixmap.data()))
    }

    fn resize(&mut self, width: u32, height: u32) {
        let w = scaled_dim(width, self.config.pixel_ratio);
        let h = scaled_dim(height, self.config.pixel_ratio);
        if let Some(pm) = Pixmap::new(w, h) {
            self.pixmap = pm;
            self.config.width = width;
            self.config.height = height;
        }
    }
}

impl SkiaBackend {
    /// Create a backend that shapes text with `text` — for example
    /// [`TextEngine::with_fonts`] with bundled fonts for output that does
    /// not depend on the machine.
    pub fn with_text_engine(config: &RenderConfig, text: TextEngine) -> RenderResult<Self> {
        validate_ratio(config.pixel_ratio)?;
        let w = scaled_dim(config.width, config.pixel_ratio);
        let h = scaled_dim(config.height, config.pixel_ratio);
        let mut pixmap = Pixmap::new(w, h).ok_or(RenderError::Target {
            width: w,
            height: h,
        })?;
        pixmap.fill(to_skia_color(&config.background)?);

        info!(width = w, height = h, "SkiaBackend initialized");
        Ok(Self {
            pixmap,
            config: config.clone(),
            text,
        })
    }

    /// Size of the render target in physical pixels.
    pub fn physical_size(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }

    /// The raw **premultiplied** RGBA8 pixels.
    pub fn premultiplied_pixels(&self) -> &[u8] {
        self.pixmap.data()
    }

    /// Encode the pixmap as PNG bytes.
    pub fn to_png(&self) -> RenderResult<Vec<u8>> {
        self.pixmap
            .encode_png()
            .map_err(|e| RenderError::Png(Box::new(e)))
    }

    /// A clip rectangle in whole physical pixels, intersected with the
    /// target: `[x, y, width, height]`, or `None` if nothing remains.
    ///
    /// Every edge is rounded to the nearest pixel boundary, so two tiles
    /// sharing an edge split the pixels along it exactly: none is drawn
    /// twice (which would double-blend translucent fills) and none is left
    /// out.
    fn clip_pixels(&self, rect: [f32; 4]) -> Option<[u32; 4]> {
        if rect.iter().any(|v| v.is_nan()) {
            return None;
        }
        let r = f64::from(self.config.pixel_ratio);
        let edge = |v: f32, max: u32| (f64::from(v) * r).round().clamp(0.0, f64::from(max)) as u32;
        let (w, h) = self.physical_size();
        let (x0, x1) = (edge(rect[0], w), edge(rect[2], w));
        let (y0, y1) = (edge(rect[1], h), edge(rect[3], h));
        (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
    }

    /// Draw a run of clipped layers (in `z_order`).
    ///
    /// Each clip rectangle is rendered into its own pixmap holding just
    /// that rectangle, so the pixmap edges are the clip and no
    /// full-target mask is ever allocated. When the run's rectangles are
    /// pairwise disjoint (the tiles of one view), all layers of a rectangle
    /// are drawn in one go: disjoint rectangles touch disjoint pixels, so
    /// regrouping cannot change the result, and only one rectangle's pixels
    /// are held at a time. Overlapping rectangles are drawn layer by layer
    /// to keep the order exact.
    fn draw_clipped(&mut self, layers: &[&RenderLayer], transform: Transform) -> RenderResult<()> {
        let keyed: Vec<(Option<[u32; 4]>, &RenderLayer)> = layers
            .iter()
            .map(|l| (l.clip.and_then(|c| self.clip_pixels(c)), *l))
            .collect();
        let mut rects: Vec<[u32; 4]> = Vec::new();
        for (rect, _) in &keyed {
            if let Some(rect) = rect
                && !rects.contains(rect)
            {
                rects.push(*rect);
            }
        }
        let overlap = |a: &[u32; 4], b: &[u32; 4]| {
            a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
        };
        let disjoint = rects
            .iter()
            .enumerate()
            .all(|(i, a)| rects[i + 1..].iter().all(|b| !overlap(a, b)));
        if disjoint {
            for rect in &rects {
                let group = keyed
                    .iter()
                    .filter(|(r, _)| r.as_ref() == Some(rect))
                    .map(|(_, l)| *l);
                self.draw_in_rect(*rect, group, transform)?;
            }
        } else {
            for (rect, layer) in &keyed {
                if let Some(rect) = rect {
                    self.draw_in_rect(*rect, std::iter::once(*layer), transform)?;
                }
            }
        }
        Ok(())
    }

    /// Draw `layers` clipped to `rect` (`[x, y, width, height]`, inside the
    /// target) through a pixmap of just that rectangle.
    fn draw_in_rect<'l>(
        &mut self,
        rect: [u32; 4],
        layers: impl Iterator<Item = &'l RenderLayer>,
        transform: Transform,
    ) -> RenderResult<()> {
        let [x, y, w, h] = rect;
        let sub_rect = i32::try_from(x)
            .ok()
            .zip(i32::try_from(y).ok())
            .and_then(|(x, y)| IntRect::from_xywh(x, y, w, h));
        let Some(mut sub) = sub_rect.and_then(|r| self.pixmap.clone_rect(r)) else {
            return Ok(());
        };
        let local = transform.post_translate(-(x as f32), -(y as f32));
        for layer in layers {
            draw_features(&mut sub.as_mut(), &layer.features, local)?;
        }
        let stride = self.pixmap.width() as usize * 4;
        let row = w as usize * 4;
        let dst = self.pixmap.data_mut();
        for (j, src) in sub.data().chunks_exact(row).enumerate() {
            let start = (y as usize + j) * stride + x as usize * 4;
            dst[start..start + row].copy_from_slice(src);
        }
        Ok(())
    }

    /// Place and draw `labels`, given in scene coordinates: a scene point
    /// `p` lands on pixel `p * scale + offset`, and lengths (font size,
    /// halo, padding, offsets) are multiplied by `scale`.
    pub fn render_labels(&mut self, labels: &[&LabelCandidate], scale: f32, offset: [f32; 2]) {
        if labels.is_empty() {
            return;
        }
        let candidates: Vec<LabelCandidate> = labels
            .iter()
            .map(|c| LabelCandidate {
                text: c.text.clone(),
                anchor: c
                    .anchor
                    .map(|p| [p[0] * scale + offset[0], p[1] * scale + offset[1]]),
                style: c.style.scaled(scale),
                layer_rank: c.layer_rank,
                sort_key: c.sort_key,
            })
            .collect();
        let (w, h) = self.physical_size();
        let placed = LabelPlacer::new(Rect::new(0.0, 0.0, w as f32, h as f32))
            .place(&mut self.text, &candidates);
        if let Some(mut canvas) = Canvas::new(self.pixmap.data_mut(), w, h) {
            self.text.draw_labels(&mut canvas, &candidates, &placed);
        }
    }
}

/// Draw the geometry of `features` (labels are placed separately).
fn draw_features(
    target: &mut PixmapMut<'_>,
    features: &[RenderFeature],
    transform: Transform,
) -> RenderResult<()> {
    for feature in features {
        match feature {
            RenderFeature::Fill { coords, color } => {
                draw_fill(target, coords, color, transform)?;
            }
            RenderFeature::Stroke {
                coords,
                color,
                width,
                cap,
                join,
                dash,
                ..
            } => {
                let stroke = StrokeStyle {
                    width: *width,
                    cap: *cap,
                    join: *join,
                    dash,
                };
                draw_stroke(target, coords, color, &stroke, transform)?;
            }
            RenderFeature::Circle {
                center,
                radius,
                color,
                stroke_color,
                stroke_width,
                ..
            } => {
                draw_circle(
                    target,
                    *center,
                    *radius,
                    color,
                    (stroke_color, *stroke_width),
                    transform,
                )?;
            }
            RenderFeature::Label(_) => {}
        }
    }
    Ok(())
}

fn draw_fill(
    target: &mut PixmapMut<'_>,
    rings: &[Vec<[f32; 2]>],
    color: &Color,
    transform: Transform,
) -> RenderResult<()> {
    let mut pb = PathBuilder::new();
    for ring in rings.iter().filter(|r| r.len() >= 3) {
        pb.move_to(ring[0][0], ring[0][1]);
        for pt in &ring[1..] {
            pb.line_to(pt[0], pt[1]);
        }
        pb.close();
    }
    if let Some(path) = pb.finish() {
        let paint = paint(color)?;
        // Even-odd so interior rings are holes regardless of winding.
        target.fill_path(&path, &paint, FillRule::EvenOdd, transform, None);
    }
    Ok(())
}

/// How a polyline is stroked.
struct StrokeStyle<'a> {
    width: f32,
    cap: LineCap,
    join: LineJoin,
    dash: &'a [f32],
}

fn draw_stroke(
    target: &mut PixmapMut<'_>,
    coords: &[[f32; 2]],
    color: &Color,
    style: &StrokeStyle<'_>,
    transform: Transform,
) -> RenderResult<()> {
    let StrokeStyle {
        width,
        cap,
        join,
        dash,
    } = *style;
    if coords.len() < 2 || width.is_nan() || width <= 0.0 {
        return Ok(());
    }
    let closed = coords.len() > 3 && coords.first() == coords.last();
    let pts = if closed {
        &coords[..coords.len() - 1]
    } else {
        coords
    };
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0][0], pts[0][1]);
    for pt in &pts[1..] {
        pb.line_to(pt[0], pt[1]);
    }
    if closed {
        pb.close();
    }
    let Some(path) = pb.finish() else {
        return Ok(());
    };

    let stroke = Stroke {
        width,
        line_cap: match cap {
            LineCap::Butt => SkiaCap::Butt,
            LineCap::Round => SkiaCap::Round,
            LineCap::Square => SkiaCap::Square,
        },
        line_join: match join {
            LineJoin::Miter => SkiaJoin::Miter,
            LineJoin::Round => SkiaJoin::Round,
            LineJoin::Bevel => SkiaJoin::Bevel,
        },
        miter_limit: MITER_LIMIT,
        // The dash is applied in path space, before `transform`, so its
        // lengths scale with the pixel ratio like the width does.
        // Patterns too fine (or too numerous) to draw are solid.
        dash: dash_is_drawable(dash, polyline_length(coords))
            .then(|| StrokeDash::new(dash.to_vec(), 0.0))
            .flatten(),
    };
    target.stroke_path(&path, &paint(color)?, &stroke, transform, None);
    Ok(())
}

fn draw_circle(
    target: &mut PixmapMut<'_>,
    center: [f32; 2],
    radius: f32,
    color: &Color,
    (stroke_color, stroke_width): (&Color, f32),
    transform: Transform,
) -> RenderResult<()> {
    if color.a > 0.0
        && let Some(disc) = circle_path(center, radius)
    {
        target.fill_path(&disc, &paint(color)?, FillRule::Winding, transform, None);
    }
    // MapLibre draws the stroke outside the radius, as a ring from `radius`
    // to `radius + stroke_width`: the path is centred within the ring.
    if stroke_width > 0.0
        && stroke_color.a > 0.0
        && let Some(ring) = circle_path(center, radius.max(0.0) + stroke_width / 2.0)
    {
        let stroke = Stroke {
            width: stroke_width,
            ..Stroke::default()
        };
        target.stroke_path(&ring, &paint(stroke_color)?, &stroke, transform, None);
    }
    Ok(())
}

fn circle_path(center: [f32; 2], radius: f32) -> Option<Path> {
    (radius > 0.0)
        .then(|| PathBuilder::from_circle(center[0], center[1], radius))
        .flatten()
}

fn paint(color: &Color) -> RenderResult<Paint<'static>> {
    let mut paint = Paint::default();
    paint.set_color(to_skia_color(color)?);
    paint.anti_alias = true;
    Ok(paint)
}

/// Convert, rejecting non-finite components; finite ones are clamped to
/// `[0, 1]`.
fn to_skia_color(c: &Color) -> RenderResult<SkiaColor> {
    if ![c.r, c.g, c.b, c.a].iter().all(|v| v.is_finite()) {
        return Err(RenderError::InvalidColor(*c));
    }
    SkiaColor::from_rgba(
        c.r.clamp(0.0, 1.0),
        c.g.clamp(0.0, 1.0),
        c.b.clamp(0.0, 1.0),
        c.a.clamp(0.0, 1.0),
    )
    .ok_or(RenderError::InvalidColor(*c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(w: u32, h: u32, ratio: f32) -> SkiaBackend {
        let config = RenderConfig {
            width: w,
            height: h,
            background: Color::TRANSPARENT,
            pixel_ratio: ratio,
        };
        let engine =
            TextEngine::with_fonts([
                include_bytes!("../tests/fonts/Cantarell-Regular.ttf").to_vec()
            ])
            .unwrap();
        SkiaBackend::with_text_engine(&config, engine).unwrap()
    }

    fn scene(features: Vec<RenderFeature>) -> SceneGraph {
        let mut layer = RenderLayer::new(0);
        layer.features = features;
        let mut s = SceneGraph::new(Color::WHITE);
        s.add_layer(layer);
        s
    }

    fn px(b: &SkiaBackend, x: u32, y: u32) -> [u8; 4] {
        let (w, _) = b.physical_size();
        let data = b.read_pixels().unwrap();
        let i = ((y * w + x) * 4) as usize;
        [data[i], data[i + 1], data[i + 2], data[i + 3]]
    }

    #[test]
    fn init_with_default_config_succeeds() {
        let config = RenderConfig::default();
        let backend = SkiaBackend::init(&config).expect("init should succeed");
        let pixels = backend.read_pixels().expect("read_pixels on fresh pixmap");
        assert_eq!(pixels.len(), (config.width * config.height * 4) as usize);
    }

    #[test]
    fn render_empty_scene_clears_to_background() {
        let mut b = backend(4, 4, 1.0);
        b.render(&SceneGraph::new(Color::rgb(1.0, 0.0, 0.0)))
            .unwrap();
        assert_eq!(px(&b, 0, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn fractional_pixel_ratios_round_not_truncate() {
        // 7 * 1.5 = 10.5 -> 11 (truncation would give 10).
        assert_eq!(backend(7, 3, 1.5).physical_size(), (11, 5));
        assert_eq!(backend(10, 10, 1.25).physical_size(), (13, 13));
        let mut b = backend(7, 7, 1.0);
        b.config.pixel_ratio = 1.5;
        b.resize(7, 3);
        assert_eq!(b.physical_size(), (11, 5));
        assert!(
            SkiaBackend::with_text_engine(
                &RenderConfig {
                    pixel_ratio: 0.0,
                    ..RenderConfig::default()
                },
                TextEngine::system()
            )
            .is_err()
        );
        assert!(
            SkiaBackend::with_text_engine(
                &RenderConfig {
                    pixel_ratio: f32::NAN,
                    ..RenderConfig::default()
                },
                TextEngine::system()
            )
            .is_err()
        );
    }

    #[test]
    fn read_pixels_returns_straight_alpha() {
        let mut b = backend(4, 4, 1.0);
        b.render(&SceneGraph::new(Color::rgba(1.0, 0.0, 0.0, 0.5)))
            .unwrap();
        let straight = px(&b, 1, 1);
        assert!(
            straight[0] >= 254 && straight[1] == 0 && straight[2] == 0,
            "{straight:?}"
        );
        assert!((i32::from(straight[3]) - 128).abs() <= 1);
        // The internal buffer is premultiplied.
        let pre = &b.premultiplied_pixels()[..4];
        assert!((i32::from(pre[0]) - 128).abs() <= 1, "{pre:?}");
    }

    #[test]
    fn even_odd_holes_stay_empty_regardless_of_winding() {
        let outer = vec![[2.0, 2.0], [18.0, 2.0], [18.0, 18.0], [2.0, 18.0]];
        // Hole wound the same way as the exterior: nonzero fill would fill it.
        let hole = vec![[7.0, 7.0], [13.0, 7.0], [13.0, 13.0], [7.0, 13.0]];
        let mut b = backend(20, 20, 1.0);
        b.render(&scene(vec![RenderFeature::Fill {
            coords: vec![outer, hole],
            color: Color::BLACK,
        }]))
        .unwrap();
        assert_eq!(px(&b, 4, 4), [0, 0, 0, 255]);
        assert_eq!(px(&b, 10, 10), [255, 255, 255, 255], "hole");
        assert_eq!(px(&b, 0, 0), [255, 255, 255, 255]);
    }

    fn line(dash: Vec<f32>, width: f32) -> RenderFeature {
        RenderFeature::Stroke {
            coords: vec![[0.0, 5.0], [40.0, 5.0]],
            color: Color::BLACK,
            width,
            width_next_zoom: width,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            dash,
        }
    }

    #[test]
    fn dash_arrays_produce_gaps() {
        let mut b = backend(40, 10, 1.0);
        b.render(&scene(vec![line(vec![8.0, 8.0], 4.0)])).unwrap();
        assert_eq!(px(&b, 4, 5)[0], 0, "inside a dash");
        assert_eq!(px(&b, 12, 5)[0], 255, "inside a gap");
        assert_eq!(px(&b, 20, 5)[0], 0, "next dash");
        let mut solid = backend(40, 10, 1.0);
        solid.render(&scene(vec![line(vec![], 4.0)])).unwrap();
        assert_eq!(px(&solid, 12, 5)[0], 0);
    }

    #[test]
    fn sharp_miters_are_beveled_at_maplibres_limit() {
        // A 37 degree apex: its miter is 3.16 half-widths long, inside
        // tiny-skia's default limit of 4 but beyond MapLibre's 2.
        let apex = RenderFeature::Stroke {
            coords: vec![[10.0, 40.0], [20.0, 10.0], [30.0, 40.0]],
            color: Color::BLACK,
            width: 4.0,
            width_next_zoom: 4.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            dash: vec![],
        };
        let mut b = backend(40, 45, 1.0);
        b.render(&scene(vec![apex])).unwrap();
        assert_eq!(px(&b, 20, 5), [255, 255, 255, 255], "miter tip cut off");
        assert_eq!(px(&b, 20, 10), [0, 0, 0, 255], "bevel present");
    }

    #[test]
    fn tiny_dashes_draw_solid_without_hanging() {
        let start = std::time::Instant::now();
        let mut b = backend(40, 10, 1.0);
        b.render(&scene(vec![line(vec![1.0e-7, 1.0e-7], 4.0)]))
            .unwrap();
        assert!(start.elapsed().as_secs() < 5);
        for x in [2, 12, 20, 35] {
            assert_eq!(px(&b, x, 5)[0], 0, "solid at {x}");
        }
    }

    #[test]
    fn pixel_ratio_scales_geometry_and_dashes() {
        let mut b = backend(40, 10, 2.0);
        b.render(&scene(vec![line(vec![8.0, 8.0], 4.0)])).unwrap();
        assert_eq!(b.physical_size(), (80, 20));
        // Dash 0..8 logical = 0..16 physical.
        assert_eq!(px(&b, 14, 10)[0], 0);
        assert_eq!(px(&b, 24, 10)[0], 255);
        // 4 logical px wide = 8 physical px tall.
        assert_eq!(px(&b, 4, 6)[0], 0);
        assert_eq!(px(&b, 4, 3)[0], 255);
    }

    #[test]
    fn circles_fill_and_outline() {
        let mut b = backend(20, 20, 1.0);
        b.render(&scene(vec![RenderFeature::Circle {
            center: [10.0, 10.0],
            radius: 5.0,
            radius_next_zoom: 5.0,
            color: Color::rgb(1.0, 0.0, 0.0),
            stroke_color: Color::rgb(0.0, 0.0, 1.0),
            stroke_width: 2.0,
        }]))
        .unwrap();
        assert_eq!(px(&b, 10, 10), [255, 0, 0, 255]);
        assert_eq!(px(&b, 0, 0), [255, 255, 255, 255]);
        let edge = px(&b, 15, 10);
        assert!(edge[2] > edge[0], "outline is blue: {edge:?}");
        // The stroke is the ring from 5 to 7 px: the pixel spanning about
        // 5..6 px from the centre is fully stroke-colored, the one spanning
        // 3..4 fully fill-colored.
        assert_eq!(px(&b, 15, 9), [0, 0, 255, 255]);
        assert_eq!(px(&b, 13, 10), [255, 0, 0, 255]);
    }

    #[test]
    fn circle_strokes_are_outside_and_independent_of_the_fill() {
        let circle = |color: Color| RenderFeature::Circle {
            center: [10.0, 10.0],
            radius: 5.0,
            radius_next_zoom: 5.0,
            color,
            stroke_color: Color::rgb(0.0, 0.0, 1.0),
            stroke_width: 2.0,
        };
        // A hollow circle (transparent fill) still draws its ring.
        let mut hollow = backend(20, 20, 1.0);
        hollow
            .render(&scene(vec![circle(Color::TRANSPARENT)]))
            .unwrap();
        assert_eq!(px(&hollow, 10, 10), [255, 255, 255, 255]);
        assert_eq!(px(&hollow, 15, 9), [0, 0, 255, 255]);
        // A translucent fill does not show the stroke through it.
        let mut translucent = backend(20, 20, 1.0);
        translucent
            .render(&scene(vec![circle(Color::rgba(1.0, 0.0, 0.0, 0.5))]))
            .unwrap();
        let inside = px(&translucent, 12, 9);
        assert!(
            inside[0] == 255 && (126..=129).contains(&inside[2]),
            "half red over white, no blue: {inside:?}"
        );
    }

    #[test]
    fn invalid_colors_are_errors_not_black() {
        let mut b = backend(4, 4, 1.0);
        let bad = Color::rgba(f32::NAN, 0.0, 0.0, 1.0);
        let err = b.render(&scene(vec![RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [3.0, 0.0], [3.0, 3.0]]],
            color: bad,
        }]));
        assert!(err.is_err());
        assert!(b.render(&SceneGraph::new(bad)).is_err());
    }

    #[test]
    fn out_of_range_components_are_clamped() {
        let c = to_skia_color(&Color::rgba(2.0, -1.0, 0.5, 3.0)).unwrap();
        assert_eq!((c.red(), c.green(), c.alpha()), (1.0, 0.0, 1.0));
    }

    #[test]
    fn render_labels_applies_its_transform() {
        use osmic_text::{LabelAnchor, LabelStyle};
        let label = LabelCandidate {
            text: "Hello".into(),
            anchor: LabelAnchor::Point([10.0, 10.0]),
            style: LabelStyle {
                font_size: 10.0,
                ..LabelStyle::default()
            },
            layer_rank: 0,
            sort_key: 0.0,
        };
        let ink = |b: &SkiaBackend| -> (u32, u32) {
            let (w, h) = b.physical_size();
            let data = b.premultiplied_pixels();
            let (mut max_x, mut max_y) = (0, 0);
            for y in 0..h {
                for x in 0..w {
                    if data[((y * w + x) * 4 + 3) as usize] > 0 {
                        max_x = max_x.max(x);
                        max_y = max_y.max(y);
                    }
                }
            }
            (max_x, max_y)
        };
        let mut small = backend(100, 100, 1.0);
        small.render_labels(&[&label], 1.0, [0.0, 0.0]);
        let mut moved = backend(100, 100, 1.0);
        moved.render_labels(&[&label], 2.0, [30.0, 0.0]);
        let (sx, sy) = ink(&small);
        let (mx, my) = ink(&moved);
        assert!(mx > sx + 30, "translated and enlarged: {sx} -> {mx}");
        assert!(my > sy, "text grew with the scale: {sy} -> {my}");
    }

    #[test]
    fn layers_are_clipped_to_their_rectangle() {
        let square = |color: Color| RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [40.0, 0.0], [40.0, 20.0], [0.0, 20.0]]],
            color,
        };
        let mut left = RenderLayer::new(0);
        left.clip = Some([0.0, 0.0, 20.0, 20.0]);
        left.push(square(Color::rgba(0.0, 0.0, 0.0, 0.5)));
        let mut right = RenderLayer::new(0);
        right.clip = Some([20.0, 0.0, 40.0, 20.0]);
        right.push(square(Color::rgba(0.0, 0.0, 0.0, 0.5)));
        let mut s = SceneGraph::new(Color::WHITE);
        s.add_layer(left);
        s.add_layer(right);
        let mut b = backend(40, 20, 1.0);
        b.render(&s).unwrap();
        // Each half is covered exactly once: no double blending anywhere.
        assert_eq!(px(&b, 5, 10), px(&b, 35, 10));
        assert!((i32::from(px(&b, 5, 10)[0]) - 128).abs() <= 1);
        // At 2x the clip scales with the pixel ratio.
        let mut b2 = backend(40, 20, 2.0);
        b2.render(&s).unwrap();
        assert_eq!(px(&b2, 10, 20), px(&b2, 70, 20));
    }

    /// Two tiles sharing a fractional edge, each covering its half with a
    /// translucent fill that overlaps into the neighbour (as tile buffers
    /// do).
    fn seam_scene(edge: f32) -> SceneGraph {
        let square = || RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [40.0, 0.0], [40.0, 20.0], [0.0, 20.0]]],
            color: Color::rgba(0.0, 0.0, 0.0, 0.5),
        };
        let mut s = SceneGraph::new(Color::WHITE);
        for clip in [[0.0, 0.0, edge, 20.0], [edge, 0.0, 40.0, 20.0]] {
            let mut tile = RenderLayer::new(0);
            tile.clip = Some(clip);
            tile.push(square());
            s.add_layer(tile);
        }
        s
    }

    #[test]
    fn adjacent_clips_with_fractional_edges_partition_pixels() {
        for (edge, ratio) in [
            (20.4, 1.0),
            (20.5, 1.0),
            (20.6, 1.0),
            (13.37, 1.5),
            (20.25, 2.0),
        ] {
            let mut b = backend(40, 20, ratio);
            b.render(&seam_scene(edge)).unwrap();
            let (w, h) = b.physical_size();
            let expected = px(&b, 0, 0);
            assert!((i32::from(expected[0]) - 128).abs() <= 1, "{expected:?}");
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(px(&b, x, y), expected, "edge {edge} @{ratio}x: ({x},{y})");
                }
            }
        }
    }

    #[test]
    fn overlapping_clips_keep_z_order() {
        let fill = |color: Color| RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [30.0, 0.0], [30.0, 10.0], [0.0, 10.0]]],
            color,
        };
        let layer = |z: i32, clip: [f32; 4], color: Color| {
            let mut l = RenderLayer::new(z);
            l.clip = Some(clip);
            l.push(fill(color));
            l
        };
        let (left, right) = ([0.0, 0.0, 20.0, 10.0], [10.0, 0.0, 30.0, 10.0]);
        let mut s = SceneGraph::new(Color::WHITE);
        s.add_layer(layer(0, left, Color::rgb(1.0, 0.0, 0.0)));
        s.add_layer(layer(1, right, Color::rgb(0.0, 1.0, 0.0)));
        s.add_layer(layer(2, left, Color::rgb(0.0, 0.0, 1.0)));
        let mut b = backend(30, 10, 1.0);
        b.render(&s).unwrap();
        // Grouping the two `left` layers would paint blue under green.
        assert_eq!(px(&b, 5, 5), [0, 0, 255, 255]);
        assert_eq!(px(&b, 15, 5), [0, 0, 255, 255], "blue is drawn last");
        assert_eq!(px(&b, 25, 5), [0, 255, 0, 255]);
    }

    #[test]
    fn clips_outside_the_target_or_nan_draw_nothing() {
        let mut s = seam_scene(20.0);
        s.layers[0].clip = Some([100.0, 100.0, 200.0, 200.0]);
        s.layers[1].clip = Some([f32::NAN, 0.0, 40.0, 20.0]);
        let mut b = backend(40, 20, 1.0);
        b.render(&s).unwrap();
        assert_eq!(px(&b, 5, 5), [255, 255, 255, 255]);
        assert_eq!(px(&b, 35, 5), [255, 255, 255, 255]);
    }

    #[test]
    fn labels_are_drawn_after_all_geometry() {
        use osmic_text::{LabelAnchor, LabelStyle};
        let mut layer = RenderLayer::new(0);
        layer.push(RenderFeature::Label(LabelCandidate {
            text: "WWWW".into(),
            anchor: LabelAnchor::Point([20.0, 10.0]),
            style: LabelStyle {
                font_size: 14.0,
                color: Color::BLACK,
                ..LabelStyle::default()
            },
            layer_rank: 0,
            sort_key: 0.0,
        }));
        let mut top = RenderLayer::new(1);
        top.push(RenderFeature::Fill {
            coords: vec![vec![[0.0, 0.0], [40.0, 0.0], [40.0, 20.0], [0.0, 20.0]]],
            color: Color::rgb(0.0, 1.0, 0.0),
        });
        let mut s = SceneGraph::new(Color::WHITE);
        s.add_layer(layer);
        s.add_layer(top);
        let mut b = backend(40, 20, 1.0);
        b.render(&s).unwrap();
        let data = b.read_pixels().unwrap();
        assert!(
            data.as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] < 60 && p[1] < 60),
            "dark text pixels over the green fill"
        );
    }
}
