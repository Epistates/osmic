//! The backend abstraction and its configuration.

use crate::error::RenderResult;
use crate::scene::SceneGraph;

/// Configuration for rendering.
///
/// Build with [`RenderConfig::new`] (or [`Default`]) and the `with_*`
/// methods.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RenderConfig {
    /// Width in logical pixels.
    pub width: u32,
    /// Height in logical pixels.
    pub height: u32,
    /// What the target holds before the first scene is rendered. (Every
    /// scene clears to its own `background`, which comes from the style.)
    pub background: osmic_core::Color,
    /// Device pixel ratio (1.0 = standard, 2.0 = retina). The target is
    /// `round(width * ratio) x round(height * ratio)` physical pixels.
    pub pixel_ratio: f32,
}

impl Default for RenderConfig {
    /// 1024 x 1024 logical pixels at ratio 1 on a transparent target.
    fn default() -> Self {
        Self::new(1024, 1024)
    }
}

impl RenderConfig {
    /// A `width` x `height` logical-pixel target at pixel ratio 1,
    /// initially transparent.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            background: osmic_core::Color::TRANSPARENT,
            pixel_ratio: 1.0,
        }
    }

    /// This configuration at device pixel ratio `ratio`.
    pub fn with_pixel_ratio(mut self, ratio: f32) -> Self {
        self.pixel_ratio = ratio;
        self
    }

    /// This configuration with the target initially filled with `color`.
    pub fn with_background(mut self, color: osmic_core::Color) -> Self {
        self.background = color;
        self
    }
}

/// Abstraction over rendering backends (software, GPU).
pub trait RenderBackend {
    /// Initialize the backend with the given configuration.
    fn init(config: &RenderConfig) -> RenderResult<Self>
    where
        Self: Sized;

    /// Render a scene graph to the internal buffer.
    fn render(&mut self, scene: &SceneGraph) -> RenderResult<()>;

    /// The rendered pixels as **straight (non-premultiplied)** RGBA8, rows
    /// top to bottom, in physical pixels.
    fn read_pixels(&self) -> Option<Vec<u8>>;

    /// Resize the render target (logical pixels).
    fn resize(&mut self, width: u32, height: u32);
}
