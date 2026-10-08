//! A small convenience wrapper: measure and rasterise a single string.

use osmic_core::Color;

use crate::blend::{Canvas, unpremultiply};
use crate::engine::TextEngine;

/// Measures text and rasterises it into a standalone RGBA buffer.
///
/// Uses the same shaping and compositing as label placement; see
/// [`TextEngine`] for the full API.
pub struct TextShaper {
    engine: TextEngine,
}

impl TextShaper {
    /// A shaper using the system fonts.
    pub fn new() -> Self {
        Self::with_engine(TextEngine::system())
    }

    /// A shaper over an existing engine (for example one built with
    /// [`TextEngine::with_fonts`] for deterministic output).
    pub fn with_engine(engine: TextEngine) -> Self {
        Self { engine }
    }

    /// Measure `text` at `font_size` pixels: `(width, height)`.
    pub fn measure(&mut self, text: &str, font_size: f32) -> (f32, f32) {
        let shaped = self.engine.shape(text, font_size);
        (shaped.width, shaped.height)
    }

    /// Rasterise `text` into a straight-alpha (non-premultiplied) RGBA
    /// buffer with a one pixel margin.
    ///
    /// Returns `(width, height, rgba_pixels)`; empty text yields `(0, 0, [])`.
    pub fn rasterize(&mut self, text: &str, font_size: f32, color: Color) -> (u32, u32, Vec<u8>) {
        let (w, h) = self.measure(text, font_size);
        if w <= 0.0 || h <= 0.0 {
            return (0, 0, Vec::new());
        }
        let (width, height) = (w.ceil() as u32 + 2, h.ceil() as u32 + 2);
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        if let Some(mut canvas) = Canvas::new(&mut pixels, width, height) {
            self.engine
                .draw_text(&mut canvas, text, 1.0, 1.0, font_size, color);
        }
        (width, height, unpremultiply(&pixels))
    }
}

impl Default for TextShaper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_engine;

    #[test]
    fn measure_scales_with_text_and_size() {
        let mut shaper = TextShaper::with_engine(test_engine());
        let (w_short, h) = shaper.measure("a", 16.0);
        let (w_long, _) = shaper.measure("aaaaaaaaaa", 16.0);
        assert!(h > 0.0 && w_short > 0.0 && w_long > w_short * 5.0);
        let (w_big, _) = shaper.measure("a", 32.0);
        assert!((w_big / w_short - 2.0).abs() < 0.2);
        assert_eq!(shaper.measure("", 16.0), (0.0, 0.0));
    }

    #[test]
    fn rasterize_returns_straight_alpha_of_the_declared_size() {
        let mut shaper = TextShaper::with_engine(test_engine());
        let (w, h, px) = shaper.rasterize("Hi", 24.0, Color::rgb(1.0, 0.0, 0.0));
        assert_eq!(px.len(), (w * h * 4) as usize);
        let opaque: Vec<&[u8; 4]> = px
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 255)
            .collect();
        assert!(!opaque.is_empty(), "stems should be fully covered");
        // Straight alpha: fully covered pixels carry the pure color, and
        // partially covered edge pixels keep it too (not darkened).
        for p in px.as_chunks::<4>().0.iter().filter(|p| p[3] > 64) {
            assert!(p[0] >= 250 && p[1] <= 4 && p[2] <= 4, "{p:?}");
        }
        assert_eq!(shaper.rasterize("   ", 24.0, Color::BLACK), (0, 0, vec![]));
    }

    #[test]
    fn text_starting_at_a_negative_offset_is_clipped_not_dropped() {
        let mut engine = test_engine();
        let mut buf = vec![0u8; 40 * 20 * 4];
        let mut canvas = Canvas::new(&mut buf, 40, 20).unwrap();
        engine.draw_text(&mut canvas, "WWWW", -12.0, 2.0, 16.0, Color::BLACK);
        let covered_cols: Vec<usize> = (0..40)
            .filter(|&x| (0..20).any(|y| buf[(y * 40 + x) * 4 + 3] > 0))
            .collect();
        assert_eq!(
            covered_cols.first(),
            Some(&0),
            "the visible tail must draw from column 0"
        );
    }
}
