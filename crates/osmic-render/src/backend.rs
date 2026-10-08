use osmic_core::error::OsmicResult;

use crate::scene::SceneGraph;

/// Configuration for rendering.
#[derive(Debug, Clone)]
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
    fn default() -> Self {
        Self {
            width: 1024,
            height: 1024,
            background: osmic_core::Color::TRANSPARENT,
            pixel_ratio: 1.0,
        }
    }
}

/// Abstraction over rendering backends (software, GPU).
pub trait RenderBackend {
    /// Initialize the backend with the given configuration.
    fn init(config: &RenderConfig) -> OsmicResult<Self>
    where
        Self: Sized;

    /// Render a scene graph to the internal buffer.
    fn render(&mut self, scene: &SceneGraph) -> OsmicResult<()>;

    /// The rendered pixels as **straight (non-premultiplied)** RGBA8, rows
    /// top to bottom, in physical pixels.
    fn read_pixels(&self) -> Option<Vec<u8>>;

    /// Resize the render target (logical pixels).
    fn resize(&mut self, width: u32, height: u32);
}
