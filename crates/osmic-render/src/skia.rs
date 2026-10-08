use cosmic_text::{Attrs, Buffer as TextBuffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{
    Color as SkiaColor, FillRule, LineCap as SkiaCap, LineJoin as SkiaJoin, Paint, PathBuilder,
    Pixmap, Stroke, Transform,
};
use tracing::info;

use osmic_core::Color;
use osmic_core::error::{OsmicError, OsmicResult};

use crate::backend::{RenderBackend, RenderConfig};
use crate::scene::{LineCap, LineJoin, RenderFeature, RenderLayer, SceneGraph};

/// Software rendering backend using tiny-skia + cosmic-text.
pub struct SkiaBackend {
    pixmap: Pixmap,
    config: RenderConfig,
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl RenderBackend for SkiaBackend {
    fn init(config: &RenderConfig) -> OsmicResult<Self> {
        let w = (config.width as f32 * config.pixel_ratio) as u32;
        let h = (config.height as f32 * config.pixel_ratio) as u32;
        let pixmap = Pixmap::new(w, h)
            .ok_or_else(|| OsmicError::Render("Failed to create pixmap".into()))?;

        info!(width = w, height = h, "SkiaBackend initialized");

        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();

        Ok(Self {
            pixmap,
            config: config.clone(),
            font_system,
            swash_cache,
        })
    }

    fn render(&mut self, scene: &SceneGraph) -> OsmicResult<()> {
        // Clear with background
        let bg = to_skia_color(&scene.background);
        self.pixmap.fill(bg);

        let scale = self.config.pixel_ratio;
        let transform = Transform::from_scale(scale, scale);

        // Sort layers by z-order
        let mut layers: Vec<&RenderLayer> = scene.layers.iter().collect();
        layers.sort_by_key(|l| l.z_order);

        for layer in layers {
            for feature in &layer.features {
                match feature {
                    RenderFeature::Fill { coords, color } => {
                        self.render_fill(coords, color, transform);
                    }
                    RenderFeature::Stroke {
                        coords,
                        color,
                        width,
                        cap,
                        join,
                    } => {
                        self.render_stroke(coords, color, *width, *cap, *join, transform);
                    }
                    RenderFeature::Label {
                        position,
                        text,
                        font_size,
                        color,
                        halo_color,
                        halo_width,
                    } => {
                        self.render_label(
                            position,
                            text,
                            *font_size,
                            color,
                            halo_color.as_ref(),
                            *halo_width,
                            transform,
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn read_pixels(&self) -> Option<Vec<u8>> {
        Some(self.pixmap.data().to_vec())
    }

    fn resize(&mut self, width: u32, height: u32) {
        let w = (width as f32 * self.config.pixel_ratio) as u32;
        let h = (height as f32 * self.config.pixel_ratio) as u32;
        if let Some(pm) = Pixmap::new(w, h) {
            self.pixmap = pm;
            self.config.width = width;
            self.config.height = height;
        }
    }
}

impl SkiaBackend {
    // All parameters are independent rendering inputs; a struct would add boilerplate for a private method.
    #[allow(clippy::too_many_arguments)]
    fn render_label(
        &mut self,
        position: &[f32; 2],
        text: &str,
        font_size: f32,
        color: &Color,
        halo_color: Option<&Color>,
        halo_width: f32,
        transform: Transform,
    ) {
        if text.is_empty() {
            return;
        }

        let scaled_size = font_size * self.config.pixel_ratio;
        let metrics = Metrics::new(scaled_size, scaled_size * 1.2);
        let mut buffer = TextBuffer::new(&mut self.font_system, metrics);
        let attrs = Attrs::new().family(Family::SansSerif);
        buffer.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.font_system, false);

        let px = position[0] * self.config.pixel_ratio;
        let py = position[1] * self.config.pixel_ratio;

        // Render halo first (wider, lighter text behind)
        if let Some(hc) = halo_color {
            let hw = (halo_width * self.config.pixel_ratio).max(1.0);
            let halo_c = cosmic_text::Color::rgba(
                (hc.r * 255.0) as u8,
                (hc.g * 255.0) as u8,
                (hc.b * 255.0) as u8,
                200,
            );
            for dy in [-hw, 0.0, hw] {
                for dx in [-hw, 0.0, hw] {
                    if dx == 0.0 && dy == 0.0 {
                        continue;
                    }
                    self.draw_text_buffer(&buffer, px + dx, py + dy, halo_c);
                }
            }
        }

        // Render text
        let text_c = cosmic_text::Color::rgba(
            (color.r * 255.0) as u8,
            (color.g * 255.0) as u8,
            (color.b * 255.0) as u8,
            255,
        );
        self.draw_text_buffer(&buffer, px, py, text_c);
        let _ = transform; // transform already applied via position
    }

    fn draw_text_buffer(
        &mut self,
        buffer: &TextBuffer,
        offset_x: f32,
        offset_y: f32,
        color: cosmic_text::Color,
    ) {
        let pw = self.pixmap.width();
        let ph = self.pixmap.height();
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            color,
            |x, y, w, h, drawn_color| {
                let alpha = drawn_color.a();
                if alpha == 0 {
                    return;
                }
                for dy in 0..h as i32 {
                    for dx in 0..w as i32 {
                        let px = (x + dx) + offset_x as i32;
                        let py = (y + dy) + offset_y as i32;
                        if px >= 0 && py >= 0 && (px as u32) < pw && (py as u32) < ph {
                            let idx = ((py as u32 * pw + px as u32) * 4) as usize;
                            let data = self.pixmap.data_mut();
                            let src_a = alpha as f32 / 255.0;
                            let dst_a = data[idx + 3] as f32 / 255.0;
                            let out_a = src_a + dst_a * (1.0 - src_a);
                            if out_a > 0.0 {
                                // Premultiplied alpha blending
                                data[idx] = ((drawn_color.r() as f32 * src_a
                                    + data[idx] as f32 * (1.0 - src_a))
                                    .min(255.0)) as u8;
                                data[idx + 1] = ((drawn_color.g() as f32 * src_a
                                    + data[idx + 1] as f32 * (1.0 - src_a))
                                    .min(255.0))
                                    as u8;
                                data[idx + 2] = ((drawn_color.b() as f32 * src_a
                                    + data[idx + 2] as f32 * (1.0 - src_a))
                                    .min(255.0))
                                    as u8;
                                data[idx + 3] = (out_a * 255.0).min(255.0) as u8;
                            }
                        }
                    }
                }
            },
        );
    }

    /// Encode the pixmap as PNG bytes.
    pub fn to_png(&self) -> OsmicResult<Vec<u8>> {
        self.pixmap
            .encode_png()
            .map_err(|e| OsmicError::Render(format!("PNG encode failed: {e}")))
    }

    fn render_fill(&mut self, rings: &[Vec<[f32; 2]>], color: &Color, transform: Transform) {
        let mut pb = PathBuilder::new();
        for ring in rings {
            if ring.len() < 3 {
                continue;
            }
            pb.move_to(ring[0][0], ring[0][1]);
            for pt in &ring[1..] {
                pb.line_to(pt[0], pt[1]);
            }
            pb.close();
        }

        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(to_skia_color(color));
            paint.anti_alias = true;
            self.pixmap
                .fill_path(&path, &paint, FillRule::EvenOdd, transform, None);
        }
    }

    fn render_stroke(
        &mut self,
        coords: &[[f32; 2]],
        color: &Color,
        width: f32,
        cap: LineCap,
        join: LineJoin,
        transform: Transform,
    ) {
        if coords.len() < 2 {
            return;
        }

        let mut pb = PathBuilder::new();
        pb.move_to(coords[0][0], coords[0][1]);
        for pt in &coords[1..] {
            pb.line_to(pt[0], pt[1]);
        }

        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(to_skia_color(color));
            paint.anti_alias = true;

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
                ..Stroke::default()
            };

            self.pixmap
                .stroke_path(&path, &paint, &stroke, transform, None);
        }
    }
}

fn to_skia_color(c: &Color) -> SkiaColor {
    SkiaColor::from_rgba(c.r, c.g, c.b, c.a).unwrap_or(SkiaColor::BLACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_with_default_config_succeeds() {
        let config = RenderConfig::default();
        let backend = SkiaBackend::init(&config).expect("init should succeed");
        let pixels = backend.read_pixels().expect("read_pixels on fresh pixmap");
        assert_eq!(
            pixels.len(),
            (config.width * config.height * 4) as usize,
            "pixel buffer should match width * height * RGBA"
        );
    }

    #[test]
    fn init_small_custom_dimensions() {
        let config = RenderConfig {
            width: 32,
            height: 16,
            background: Color::rgba(0.0, 0.0, 0.0, 1.0),
            pixel_ratio: 1.0,
        };
        let backend = SkiaBackend::init(&config).expect("small init ok");
        let pixels = backend.read_pixels().unwrap();
        assert_eq!(pixels.len(), 32 * 16 * 4);
    }

    #[test]
    fn render_empty_scene_clears_to_background() {
        let config = RenderConfig {
            width: 4,
            height: 4,
            background: Color::rgba(1.0, 0.0, 0.0, 1.0),
            pixel_ratio: 1.0,
        };
        let mut backend = SkiaBackend::init(&config).unwrap();
        let scene = SceneGraph {
            background: Color::rgba(1.0, 0.0, 0.0, 1.0),
            layers: vec![],
        };
        backend.render(&scene).expect("render empty scene");
        let pixels = backend.read_pixels().unwrap();
        // First pixel's red channel should be saturated.
        assert_eq!(pixels[0], 255, "background red channel should be 255");
    }
}
