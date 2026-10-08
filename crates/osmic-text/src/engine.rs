//! Text shaping and glyph rasterisation on top of cosmic-text.

use std::collections::HashMap;
use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent, fontdb,
};
use osmic_core::Color;

use crate::blend::{Canvas, Mask};
use crate::label::{LabelCandidate, PlacedGlyph, PlacedLabel};

/// Line height as a multiple of the font size.
const LINE_HEIGHT: f32 = 1.2;

/// Shaped-text cache bound; the cache is cleared when it is exceeded.
const SHAPE_CACHE_LIMIT: usize = 8192;

/// Largest label bitmap, per side, that will be rasterised.
const MAX_LABEL_SIDE: i64 = 4096;

/// Largest font size, in pixels, that is shaped; larger text is empty.
/// (Glyph bitmaps grow with the square of the size.)
pub const MAX_FONT_SIZE: f32 = 2048.0;

/// Glyph positions beyond this many pixels from the origin are not
/// rasterised: `f32` stops representing whole pixels exactly there, and it
/// keeps all bitmap arithmetic far from `i32` overflow.
const MAX_COORDINATE: f32 = 16_777_216.0;

/// No usable font could be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoFontsError;

impl std::fmt::Display for NoFontsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no usable font in the supplied font data")
    }
}

impl std::error::Error for NoFontsError {}

/// One shaped glyph, positioned along a single baseline.
#[derive(Debug, Clone)]
pub struct ShapedGlyph {
    key: CacheKey,
    /// Pen position of the glyph's left edge, in pixels from the start of
    /// the text.
    pub x: f32,
    /// Horizontal advance in pixels.
    pub advance: f32,
    /// Integer glyph position within the line box (from cosmic-text).
    ix: i32,
    iy: i32,
}

/// Text shaped at one size on a single line.
#[derive(Debug, Clone)]
pub struct ShapedText {
    /// The glyphs, in visual order.
    pub glyphs: Vec<ShapedGlyph>,
    /// Width of the line in pixels.
    pub width: f32,
    /// Height of the line box in pixels.
    pub height: f32,
    /// The font size it was shaped at, in pixels (0 if empty).
    pub font_size: f32,
}

/// The widest halo drawn around text of `font_size` pixels: a quarter of
/// the size, MapLibre's effective limit (its glyph SDFs carry no more).
/// Wider `text-halo-width`s are clamped to it.
pub fn max_halo_width(font_size: f32) -> f32 {
    font_size / 4.0
}

/// `halo_width` limited to `[0, max_halo_width(font_size)]`; NaN is 0.
fn effective_halo(halo_width: f32, font_size: f32) -> f32 {
    if halo_width > 0.0 {
        halo_width.min(max_halo_width(font_size))
    } else {
        0.0
    }
}

impl ShapedText {
    fn empty() -> Self {
        Self {
            glyphs: Vec::new(),
            width: 0.0,
            height: 0.0,
            font_size: 0.0,
        }
    }

    /// Whether nothing would be drawn.
    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    /// Centre of glyph `i`'s box in line-box coordinates.
    pub(crate) fn glyph_center(&self, i: usize) -> [f32; 2] {
        let g = &self.glyphs[i];
        [g.x + g.advance / 2.0, self.height / 2.0]
    }
}

/// A rasterised glyph's coverage, positioned relative to its line-box
/// coordinates.
struct GlyphBitmap {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    alpha: Vec<u8>,
}

/// A label rasterised into coverage masks, ready to composite.
#[derive(Debug, Clone)]
pub struct LabelBitmap {
    /// Canvas position of the masks' top-left corner.
    pub x: i32,
    pub y: i32,
    /// Glyph coverage.
    pub text: Mask,
    /// Glyph coverage dilated by the halo width, if there is a halo.
    pub halo: Option<Mask>,
}

impl LabelBitmap {
    /// Composite the halo (if any) and then the text onto `canvas`.
    pub fn composite(&self, canvas: &mut Canvas<'_>, color: Color, halo_color: Color) {
        if let Some(halo) = &self.halo {
            canvas.composite_mask(halo, self.x, self.y, halo_color);
        }
        canvas.composite_mask(&self.text, self.x, self.y, color);
    }
}

/// Shapes text and rasterises glyphs.
///
/// Shaping results are cached per `(text, size)`, and glyph images come
/// from cosmic-text's `SwashCache`, so repeated frames over the same labels
/// do almost no work.
pub struct TextEngine {
    font_system: FontSystem,
    swash: SwashCache,
    shapes: HashMap<(String, u32), Arc<ShapedText>>,
}

impl TextEngine {
    /// An engine using the fonts installed on the system.
    pub fn system() -> Self {
        Self::from_font_system(FontSystem::new())
    }

    /// An engine that uses **only** the supplied font files (TTF/OTF),
    /// ignoring system fonts, so output is identical on every machine.
    /// The first font becomes the sans-serif default.
    pub fn with_fonts(fonts: impl IntoIterator<Item = Vec<u8>>) -> Result<Self, NoFontsError> {
        let mut db = fontdb::Database::new();
        for data in fonts {
            db.load_font_data(data);
        }
        let family = db
            .faces()
            .next()
            .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
            .ok_or(NoFontsError)?;
        db.set_sans_serif_family(&family);
        db.set_serif_family(&family);
        db.set_monospace_family(&family);
        db.set_cursive_family(&family);
        db.set_fantasy_family(&family);
        Ok(Self::from_font_system(FontSystem::new_with_locale_and_db(
            "en-US".to_string(),
            db,
        )))
    }

    fn from_font_system(font_system: FontSystem) -> Self {
        Self {
            font_system,
            swash: SwashCache::new(),
            shapes: HashMap::new(),
        }
    }

    /// Shape `text` on one line at `font_size` pixels. Sizes outside
    /// `1..=MAX_FONT_SIZE` (or NaN) give empty text.
    pub fn shape(&mut self, text: &str, font_size: f32) -> Arc<ShapedText> {
        if !(1.0..=MAX_FONT_SIZE).contains(&font_size) || text.trim().is_empty() {
            return Arc::new(ShapedText::empty());
        }
        let key = (text.to_string(), font_size.to_bits());
        if let Some(hit) = self.shapes.get(&key) {
            return Arc::clone(hit);
        }
        if self.shapes.len() >= SHAPE_CACHE_LIMIT {
            self.shapes.clear();
        }
        let shaped = Arc::new(self.shape_uncached(text, font_size));
        self.shapes.insert(key, Arc::clone(&shaped));
        shaped
    }

    fn shape_uncached(&mut self, text: &str, font_size: f32) -> ShapedText {
        let fs = &mut self.font_system;
        let mut buffer = Buffer::new(fs, Metrics::new(font_size, font_size * LINE_HEIGHT));
        buffer.set_size(None, None);
        let single_line = text.replace(['\n', '\r'], " ");
        buffer.set_text(
            &single_line,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(fs, false);

        let mut shaped = ShapedText::empty();
        shaped.font_size = font_size;
        for run in buffer.layout_runs() {
            shaped.width = shaped.width.max(run.line_w);
            shaped.height = shaped.height.max(run.line_height);
            for glyph in run.glyphs {
                let physical = glyph.physical((0.0, run.line_y), 1.0);
                shaped.glyphs.push(ShapedGlyph {
                    key: physical.cache_key,
                    x: glyph.x,
                    advance: glyph.w,
                    ix: physical.x,
                    iy: physical.y,
                });
            }
        }
        shaped
    }

    /// Coverage bitmap of one glyph, from the swash cache.
    fn glyph_bitmap(&mut self, key: CacheKey) -> Option<GlyphBitmap> {
        let image = self.swash.get_image(&mut self.font_system, key).as_ref()?;
        let (w, h) = (image.placement.width, image.placement.height);
        let alpha = match image.content {
            SwashContent::Mask => image.data.clone(),
            // LCD subpixel masks: average the three channels.
            SwashContent::SubpixelMask => image
                .data
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| ((u16::from(c[0]) + u16::from(c[1]) + u16::from(c[2])) / 3) as u8)
                .collect(),
            SwashContent::Color => image.data.as_chunks::<4>().0.iter().map(|c| c[3]).collect(),
        };
        (alpha.len() == w as usize * h as usize).then_some(GlyphBitmap {
            left: image.placement.left,
            top: image.placement.top,
            width: w,
            height: h,
            alpha,
        })
    }

    /// Rasterise a placed label into coverage masks.
    ///
    /// `glyphs` carry the on-canvas position and rotation of each glyph;
    /// `halo_width` (pixels) controls how far the halo mask is dilated. It
    /// is clamped to [`max_halo_width`] of the shaped size.
    pub fn rasterize(
        &mut self,
        shaped: &ShapedText,
        glyphs: &[PlacedGlyph],
        halo_width: f32,
    ) -> Option<LabelBitmap> {
        let halo_width = effective_halo(halo_width, shaped.font_size);
        struct Item {
            bitmap: GlyphBitmap,
            center_local: [f32; 2],
            at: [f32; 2],
            angle: f32,
            ix: i32,
            iy: i32,
        }
        let mut items = Vec::with_capacity(glyphs.len());
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for g in glyphs {
            let sg = shaped.glyphs.get(g.index)?;
            let placeable = g.angle.is_finite()
                && g.center
                    .iter()
                    .all(|c| c.is_finite() && c.abs() <= MAX_COORDINATE);
            if !placeable {
                return None;
            }
            let Some(bitmap) = self.glyph_bitmap(sg.key) else {
                continue; // blank glyph (space) or missing from the font
            };
            let center_local = shaped.glyph_center(g.index);
            let item = Item {
                bitmap,
                center_local,
                at: g.center,
                angle: g.angle,
                ix: sg.ix,
                iy: sg.iy,
            };
            for corner in item.corners() {
                for k in 0..2 {
                    lo[k] = lo[k].min(corner[k]);
                    hi[k] = hi[k].max(corner[k]);
                }
            }
            items.push(item);
        }
        if items.is_empty() {
            return None;
        }

        impl Item {
            /// Local (line-box) position of the bitmap's top-left.
            fn local_origin(&self) -> [f32; 2] {
                [
                    (self.ix + self.bitmap.left) as f32,
                    (self.iy - self.bitmap.top) as f32,
                ]
            }

            /// Map a line-box point to canvas space.
            fn to_canvas(&self, p: [f32; 2]) -> [f32; 2] {
                let (s, c) = self.angle.sin_cos();
                let d = [p[0] - self.center_local[0], p[1] - self.center_local[1]];
                [
                    self.at[0] + d[0] * c - d[1] * s,
                    self.at[1] + d[0] * s + d[1] * c,
                ]
            }

            fn corners(&self) -> [[f32; 2]; 4] {
                let o = self.local_origin();
                let (w, h) = (self.bitmap.width as f32, self.bitmap.height as f32);
                [
                    self.to_canvas(o),
                    self.to_canvas([o[0] + w, o[1]]),
                    self.to_canvas([o[0], o[1] + h]),
                    self.to_canvas([o[0] + w, o[1] + h]),
                ]
            }
        }

        // Glyph centres are within `MAX_COORDINATE` and glyphs within
        // `MAX_FONT_SIZE`, so these fit easily in `i64`; once the box passes
        // the size check every coordinate fits in `i32` too.
        let pad = halo_width.ceil() as i64 + 1;
        let x0 = lo[0].floor() as i64 - pad;
        let y0 = lo[1].floor() as i64 - pad;
        let x1 = hi[0].ceil() as i64 + pad;
        let y1 = hi[1].ceil() as i64 + pad;
        if x1 - x0 > MAX_LABEL_SIDE || y1 - y0 > MAX_LABEL_SIDE || x1 <= x0 || y1 <= y0 {
            return None;
        }
        let (x0, y0) = (i32::try_from(x0).ok()?, i32::try_from(y0).ok()?);
        let mut mask = Mask::new((x1 - i64::from(x0)) as u32, (y1 - i64::from(y0)) as u32);

        for item in &items {
            let bm = &item.bitmap;
            let o = item.local_origin();
            if item.angle.abs() < 1e-4 {
                // Axis-aligned: copy pixels at an integer offset.
                let dx = (item.at[0] - item.center_local[0]).round() as i32 + o[0] as i32 - x0;
                let dy = (item.at[1] - item.center_local[1]).round() as i32 + o[1] as i32 - y0;
                for j in 0..bm.height as i32 {
                    for i in 0..bm.width as i32 {
                        mask.add(
                            dx + i,
                            dy + j,
                            bm.alpha[(j as u32 * bm.width + i as u32) as usize],
                        );
                    }
                }
            } else {
                // Rotated: sample the glyph bilinearly through the inverse
                // transform for every destination pixel it can touch.
                let corners = item.corners();
                let (mut cl, mut ch) = ([f32::MAX; 2], [f32::MIN; 2]);
                for c in corners {
                    for k in 0..2 {
                        cl[k] = cl[k].min(c[k]);
                        ch[k] = ch[k].max(c[k]);
                    }
                }
                let (s, c) = (-item.angle).sin_cos();
                for py in (cl[1].floor() as i32)..(ch[1].ceil() as i32) {
                    for px in (cl[0].floor() as i32)..(ch[0].ceil() as i32) {
                        let d = [px as f32 + 0.5 - item.at[0], py as f32 + 0.5 - item.at[1]];
                        let local = [
                            item.center_local[0] + d[0] * c - d[1] * s,
                            item.center_local[1] + d[0] * s + d[1] * c,
                        ];
                        let u = local[0] - o[0] - 0.5;
                        let v = local[1] - o[1] - 0.5;
                        let cov = sample_bilinear(&bm.alpha, bm.width, bm.height, u, v);
                        mask.add(px - x0, py - y0, cov);
                    }
                }
            }
        }

        let halo = (halo_width > 0.0).then(|| mask.dilate(halo_width));
        Some(LabelBitmap {
            x: x0,
            y: y0,
            text: mask,
            halo,
        })
    }

    /// Rasterise and composite every placed label onto `canvas`.
    ///
    /// `placed[i].candidate` indexes into `candidates`.
    pub fn draw_labels(
        &mut self,
        canvas: &mut Canvas<'_>,
        candidates: &[LabelCandidate],
        placed: &[PlacedLabel],
    ) {
        for label in placed {
            let cand = &candidates[label.candidate];
            let shaped = self.shape(&cand.text, cand.style.font_size);
            if let Some(bitmap) = self.rasterize(&shaped, &label.glyphs, cand.style.halo_width) {
                bitmap.composite(canvas, cand.style.color, cand.style.halo_color);
            }
        }
    }

    /// Draw `text` with its top-left corner at `(x, y)`; no placement or
    /// collision handling (for UI text).
    pub fn draw_text(
        &mut self,
        canvas: &mut Canvas<'_>,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: Color,
    ) {
        let shaped = self.shape(text, font_size);
        let glyphs: Vec<PlacedGlyph> = (0..shaped.glyphs.len())
            .map(|index| {
                let c = shaped.glyph_center(index);
                PlacedGlyph {
                    index,
                    center: [x.round() + c[0], y.round() + c[1]],
                    angle: 0.0,
                }
            })
            .collect();
        if let Some(bitmap) = self.rasterize(&shaped, &glyphs, 0.0) {
            bitmap.composite(canvas, color, Color::TRANSPARENT);
        }
    }
}

fn sample_bilinear(data: &[u8], w: u32, h: u32, u: f32, v: f32) -> u8 {
    let (x0, y0) = (u.floor(), v.floor());
    let (fx, fy) = (u - x0, v - y0);
    let at = |x: i32, y: i32| -> f32 {
        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
            0.0
        } else {
            f32::from(data[y as usize * w as usize + x as usize])
        }
    };
    let (x0, y0) = (x0 as i32, y0 as i32);
    let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
    let bottom = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
    (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{LabelAnchor, LabelPlacer, LabelStyle};
    use crate::test_support::test_engine;
    use crate::{Canvas, Rect};

    fn count(mask: &Mask) -> usize {
        mask.data.iter().filter(|&&c| c > 0).count()
    }

    #[test]
    fn shaping_is_cached_and_deterministic() {
        let mut a = test_engine();
        let mut b = test_engine();
        let x = a.shape("Cache me", 14.0);
        let y = a.shape("Cache me", 14.0);
        assert!(Arc::ptr_eq(&x, &y));
        assert_eq!(x.width, b.shape("Cache me", 14.0).width);
        assert!(a.shape("", 14.0).is_empty());
        assert!(a.shape("x", 0.0).is_empty());
    }

    #[test]
    fn engine_without_usable_fonts_is_an_error() {
        assert!(TextEngine::with_fonts([b"not a font".to_vec()]).is_err());
        assert!(TextEngine::with_fonts(std::iter::empty()).is_err());
    }

    #[test]
    fn halo_is_a_dilation_of_the_text() {
        let mut engine = test_engine();
        let shaped = engine.shape("Halo", 24.0);
        let glyphs: Vec<PlacedGlyph> = (0..shaped.glyphs.len())
            .map(|index| {
                let c = shaped.glyph_center(index);
                PlacedGlyph {
                    index,
                    center: [20.0 + c[0], 20.0 + c[1]],
                    angle: 0.0,
                }
            })
            .collect();
        let plain = engine.rasterize(&shaped, &glyphs, 0.0).unwrap();
        let haloed = engine.rasterize(&shaped, &glyphs, 3.0).unwrap();
        assert!(plain.halo.is_none());
        let halo = haloed.halo.as_ref().unwrap();
        assert!(count(halo) > count(&haloed.text) * 2);
        // The halo contains every text pixel.
        for y in 0..haloed.text.height as i32 {
            for x in 0..haloed.text.width as i32 {
                assert!(halo.get(x, y) >= haloed.text.get(x, y));
            }
        }
    }

    #[test]
    fn huge_halos_are_clamped_to_a_quarter_of_the_font_size() {
        let mut engine = test_engine();
        let shaped = engine.shape("Halo", 16.0);
        let glyphs: Vec<PlacedGlyph> = (0..shaped.glyphs.len())
            .map(|index| {
                let c = shaped.glyph_center(index);
                PlacedGlyph {
                    index,
                    center: [50.0 + c[0], 50.0 + c[1]],
                    angle: 0.0,
                }
            })
            .collect();
        let start = std::time::Instant::now();
        let huge = engine.rasterize(&shaped, &glyphs, 1.0e9).unwrap();
        assert!(start.elapsed().as_secs() < 5, "must not hang");
        let capped = engine.rasterize(&shaped, &glyphs, 4.0).unwrap();
        assert_eq!(max_halo_width(16.0), 4.0);
        assert_eq!(
            (huge.text.width, huge.text.height),
            (capped.text.width, capped.text.height),
            "padded for the clamped halo only"
        );
        assert_eq!(huge.halo, capped.halo);
        assert!(
            engine
                .rasterize(&shaped, &glyphs, f32::NAN)
                .unwrap()
                .halo
                .is_none()
        );
    }

    #[test]
    fn huge_or_invalid_positions_are_skipped_not_overflowed() {
        let mut engine = test_engine();
        let mut buf = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::new(&mut buf, 20, 20).unwrap();
        for (x, y) in [
            (f32::MAX, 0.0),
            (-f32::MAX, -f32::MAX),
            (3.0e9, 3.0e9),
            (f32::NAN, 1.0),
            (f32::INFINITY, 1.0),
        ] {
            engine.draw_text(&mut canvas, "Far", x, y, 12.0, Color::BLACK);
        }
        let shaped = engine.shape("Far", 12.0);
        let glyph = |center, angle| PlacedGlyph {
            index: 0,
            center,
            angle,
        };
        assert!(
            engine
                .rasterize(&shaped, &[glyph([1.0e30, 0.0], 0.0)], 2.0)
                .is_none()
        );
        assert!(
            engine
                .rasterize(&shaped, &[glyph([5.0, 5.0], f32::NAN)], 0.0)
                .is_none()
        );
        assert!(
            engine
                .rasterize(&shaped, &[glyph([5.0, 5.0], 0.0)], 0.0)
                .is_some()
        );
        assert!(buf.iter().all(|&b| b == 0), "nothing drawn");
        // Absurd font sizes shape to nothing instead of huge bitmaps.
        assert!(engine.shape("Big", MAX_FONT_SIZE * 2.0).is_empty());
        assert!(!engine.shape("Big", MAX_FONT_SIZE).is_empty());
    }

    #[test]
    fn rotated_text_covers_the_rotated_extent() {
        let mut engine = test_engine();
        let shaped = engine.shape("Rotated", 24.0);
        let angle = std::f32::consts::FRAC_PI_4;
        let (s, c) = angle.sin_cos();
        let glyphs: Vec<PlacedGlyph> = (0..shaped.glyphs.len())
            .map(|index| {
                let d = shaped.glyphs[index].x + shaped.glyphs[index].advance / 2.0
                    - shaped.width / 2.0;
                PlacedGlyph {
                    index,
                    center: [100.0 + d * c, 100.0 + d * s],
                    angle,
                }
            })
            .collect();
        let bm = engine.rasterize(&shaped, &glyphs, 0.0).unwrap();
        let (w, h) = (bm.text.width as f32, bm.text.height as f32);
        // A text 45 degrees off horizontal is about as tall as it is wide.
        assert!((w - h).abs() < w * 0.25, "{w}x{h}");
        assert!(count(&bm.text) > 100);
    }

    #[test]
    fn draw_labels_composites_placed_labels() {
        let mut engine = test_engine();
        let candidates = [LabelCandidate {
            text: "Label".into(),
            anchor: LabelAnchor::Point([50.0, 20.0]),
            style: LabelStyle {
                font_size: 16.0,
                color: Color::rgb(1.0, 0.0, 0.0),
                halo_color: Color::WHITE,
                halo_width: 2.0,
                ..LabelStyle::default()
            },
            layer_rank: 0,
            sort_key: 0.0,
        }];
        let placed =
            LabelPlacer::new(Rect::new(0.0, 0.0, 100.0, 40.0)).place(&mut engine, &candidates);
        let mut buf = vec![0u8; 100 * 40 * 4];
        let mut canvas = Canvas::new(&mut buf, 100, 40).unwrap();
        engine.draw_labels(&mut canvas, &candidates, &placed);
        let px: Vec<&[u8; 4]> = buf.as_chunks::<4>().0.iter().collect();
        assert!(
            px.iter().any(|p| p[3] == 255 && p[0] > 240 && p[1] < 20),
            "red text pixels"
        );
        assert!(
            px.iter().any(|p| p[3] > 200 && p[0] > 240 && p[1] > 240),
            "white halo pixels"
        );
        // Premultiplied invariant: no channel exceeds alpha.
        assert!(px.iter().all(|p| p[..3].iter().all(|&c| c <= p[3])));
    }
}
