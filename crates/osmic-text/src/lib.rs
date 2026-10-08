//! Text for maps: shaping, label placement and glyph rasterisation.
//!
//! * [`TextEngine`] shapes text with cosmic-text (results are kept in an
//!   LRU cache per text, size and [`FontStack`]) and rasterises glyphs
//!   through cosmic-text's `SwashCache`, which is the glyph cache. Use
//!   [`TextEngine::with_fonts`] to render with bundled fonts only
//!   (deterministic output) or [`TextEngine::system`] for the installed
//!   fonts. A [`FontStack`] (MapLibre `text-font`) picks the first loaded
//!   family, weight and style it names.
//! * [`LabelPlacer`] places [`LabelCandidate`]s without overlaps, in a
//!   deterministic priority order, using rectangles taken from the real
//!   shaped extents and an R-tree [`CollisionIndex`]. Labels attached to a
//!   polyline follow it glyph by glyph.
//! * Halos are produced by dilating the glyph coverage mask
//!   ([`Mask::dilate`]), and all glyph compositing goes through one
//!   premultiplied-alpha routine ([`Canvas::composite_mask`]).
//!
//! There is no GPU glyph atlas: consumers composite the CPU-rasterised
//! labels, for example as an overlay texture.

mod blend;
mod collision;
mod engine;
mod font;
mod label;
mod shaping;

pub use blend::{Canvas, Mask, blend_over, unpremultiply};
pub use collision::{CollisionIndex, Rect};
pub use engine::{
    LabelBitmap, MAX_FONT_SIZE, NoFontsError, ShapedGlyph, ShapedText, TextEngine, max_halo_width,
};
pub use font::FontStack;
pub use label::{
    LabelAnchor, LabelCandidate, LabelPlacer, LabelStyle, PlacedGlyph, PlacedLabel, clip_polyline,
};
pub use shaping::TextShaper;

#[cfg(test)]
mod test_support {
    use crate::TextEngine;

    /// An engine over the bundled OFL test font, identical on every machine.
    pub fn test_engine() -> TextEngine {
        TextEngine::with_fonts([include_bytes!("../tests/fonts/Cantarell-Regular.ttf").to_vec()])
            .expect("bundled font loads")
    }
}
