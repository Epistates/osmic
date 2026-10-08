//! Rendering errors.

use osmic_core::Color;

/// Errors from creating a render target or rendering into it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenderError {
    /// `pixel_ratio` must be finite and positive.
    #[error("pixel_ratio must be a positive number, got {0}")]
    InvalidPixelRatio(f32),
    /// The render target could not be allocated (zero-sized or too large).
    #[error("cannot create a {width}x{height} render target")]
    Target { width: u32, height: u32 },
    /// A color with non-finite components.
    #[error("invalid color {0:?}")]
    InvalidColor(Color),
    /// Encoding the image as PNG failed.
    #[error("PNG encoding failed")]
    Png(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Result alias for [`RenderError`].
pub type RenderResult<T> = Result<T, RenderError>;
