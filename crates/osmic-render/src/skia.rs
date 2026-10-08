use std::collections::HashMap;

use tiny_skia::{
    Color as SkiaColor, FillRule, LineCap as SkiaCap, LineJoin as SkiaJoin, Mask, Paint, Path,
    PathBuilder, Pixmap, Stroke, StrokeDash, Transform,
};
use tracing::info;

use osmic_core::Color;
use osmic_text::{Canvas, LabelCandidate, LabelPlacer, Rect, TextEngine, unpremultiply};

use crate::backend::{RenderBackend, RenderConfig};
use crate::error::{RenderError, RenderResult};
use crate::scene::{LineCap, LineJoin, RenderFeature, RenderLayer, SceneGraph};
use crate::tessellate::{dash_is_drawable, polyline_length};

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

        let mut labels: Vec<&LabelCandidate> = Vec::new();
        // Clip masks are shared by all layers clipped to the same rectangle
        // (every style layer of one tile).
        let mut masks: HashMap<[i32; 4], Option<Mask>> = HashMap::new();
        for layer in layers {
            let mask = match layer.clip {
                Some(rect) => {
                    let key = self.clip_key(rect);
                    masks
                        .entry(key)
                        .or_insert_with(|| self.clip_mask(key))
                        .as_ref()
                }
                None => None,
            };
            for feature in &layer.features {
                match feature {
                    RenderFeature::Fill { coords, color } => {
                        self.render_fill(coords, color, transform, mask)?;
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
                        self.render_stroke(
                            coords, color, *width, *cap, *join, dash, transform, mask,
                        )?;
                    }
                    RenderFeature::Circle {
                        center,
                        radius,
                        color,
                        stroke_color,
                        stroke_width,
                        ..
                    } => {
                        self.render_circle(
                            *center,
                            *radius,
                            color,
                            stroke_color,
                            *stroke_width,
                            transform,
                            mask,
                        )?;
                    }
                    RenderFeature::Label(candidate) => labels.push(candidate),
                }
            }
        }
        // Labels are placed across all layers at once so priorities apply,
        // then drawn on top of the geometry.
        self.render_labels(&labels, transform);
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

    /// A clip rectangle in whole physical pixels, grown outward so adjacent
    /// tiles overlap by a sub-pixel at most instead of leaving gaps.
    fn clip_key(&self, rect: [f32; 4]) -> [i32; 4] {
        let r = self.config.pixel_ratio;
        [
            (rect[0] * r).floor() as i32,
            (rect[1] * r).floor() as i32,
            (rect[2] * r).ceil() as i32,
            (rect[3] * r).ceil() as i32,
        ]
    }

    fn clip_mask(&self, key: [i32; 4]) -> Option<Mask> {
        let mut mask = Mask::new(self.pixmap.width(), self.pixmap.height())?;
        let rect =
            tiny_skia::Rect::from_ltrb(key[0] as f32, key[1] as f32, key[2] as f32, key[3] as f32)?;
        mask.fill_path(
            &PathBuilder::from_rect(rect),
            FillRule::Winding,
            false,
            Transform::identity(),
        );
        Some(mask)
    }

    /// Place and draw `labels` (scene coordinates). `transform` maps scene
    /// coordinates to pixels; it must be a scale plus translation — label
    /// anchors are transformed by it and lengths (font size, halo, padding)
    /// are multiplied by its scale.
    pub fn render_labels(&mut self, labels: &[&LabelCandidate], transform: Transform) {
        if labels.is_empty() {
            return;
        }
        let (scale, tx, ty) = (transform.sx, transform.tx, transform.ty);
        let candidates: Vec<LabelCandidate> = labels
            .iter()
            .map(|c| LabelCandidate {
                anchor: c.anchor.map(|p| [p[0] * scale + tx, p[1] * scale + ty]),
                style: c.style.scaled(scale),
                ..(*c).clone()
            })
            .collect();
        let (w, h) = self.physical_size();
        let placed = LabelPlacer::new(Rect::new(0.0, 0.0, w as f32, h as f32))
            .place(&mut self.text, &candidates);
        if let Some(mut canvas) = Canvas::new(self.pixmap.data_mut(), w, h) {
            self.text.draw_labels(&mut canvas, &candidates, &placed);
        }
    }

    fn render_fill(
        &mut self,
        rings: &[Vec<[f32; 2]>],
        color: &Color,
        transform: Transform,
        mask: Option<&Mask>,
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
            self.pixmap
                .fill_path(&path, &paint, FillRule::EvenOdd, transform, mask);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // independent stroke parameters
    fn render_stroke(
        &mut self,
        coords: &[[f32; 2]],
        color: &Color,
        width: f32,
        cap: LineCap,
        join: LineJoin,
        dash: &[f32],
        transform: Transform,
        mask: Option<&Mask>,
    ) -> RenderResult<()> {
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
            // The dash is applied in path space, before `transform`, so its
            // lengths scale with the pixel ratio like the width does.
            // Patterns too fine (or too numerous) to draw are solid.
            dash: dash_is_drawable(dash, polyline_length(coords))
                .then(|| StrokeDash::new(dash.to_vec(), 0.0))
                .flatten(),
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint(color)?, &stroke, transform, mask);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // independent circle parameters
    fn render_circle(
        &mut self,
        center: [f32; 2],
        radius: f32,
        color: &Color,
        stroke_color: &Color,
        stroke_width: f32,
        transform: Transform,
        mask: Option<&Mask>,
    ) -> RenderResult<()> {
        let Some(path) = circle_path(center, radius) else {
            return Ok(());
        };
        if color.a > 0.0 {
            self.pixmap
                .fill_path(&path, &paint(color)?, FillRule::Winding, transform, mask);
        }
        if stroke_width > 0.0 && stroke_color.a > 0.0 {
            let stroke = Stroke {
                width: stroke_width,
                ..Stroke::default()
            };
            self.pixmap
                .stroke_path(&path, &paint(stroke_color)?, &stroke, transform, mask);
        }
        Ok(())
    }
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
        small.render_labels(&[&label], Transform::identity());
        let mut moved = backend(100, 100, 1.0);
        moved.render_labels(
            &[&label],
            Transform::from_scale(2.0, 2.0).post_translate(30.0, 0.0),
        );
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
