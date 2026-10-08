//! The backend-agnostic scene: what to draw, already styled and projected.
//!
//! A [`SceneGraph`] is produced by [`crate::SceneBuilder`] (style +
//! features → scene) and consumed by a [`crate::RenderBackend`] (software
//! rasterisation) or by [`crate::tessellate_scene`] (GPU meshes). Coordinates
//! are pixels in whatever space the builder's [`crate::PixelMapping`]
//! defined.

use osmic_core::Color;
use osmic_text::LabelCandidate;

pub use osmic_style::{LineCap, LineJoin};

/// A styled, projected drawing.
#[derive(Debug, Clone)]
pub struct SceneGraph {
    /// Color the canvas is cleared to.
    pub background: Color,
    pub layers: Vec<RenderLayer>,
}

impl SceneGraph {
    pub fn new(background: Color) -> Self {
        Self {
            background,
            layers: Vec::new(),
        }
    }

    pub fn add_layer(&mut self, layer: RenderLayer) {
        self.layers.push(layer);
    }

    /// Append `other`'s layers (its background is dropped). Used to compose
    /// a view from per-tile scenes.
    pub fn append(&mut self, other: SceneGraph) {
        self.layers.extend(other.layers);
    }

    /// Total number of drawing primitives.
    pub fn feature_count(&self) -> usize {
        self.layers.iter().map(|l| l.features.len()).sum()
    }
}

/// The primitives of one style layer. Layers draw in ascending `z_order`
/// (ties keep their order); for scenes built from a style it is the layer's
/// index in the style.
#[derive(Debug, Clone)]
pub struct RenderLayer {
    pub z_order: i32,
    /// Rectangle `[x0, y0, x1, y1]` the layer's geometry is clipped to.
    /// Per-tile scenes set it to the tile bounds so features that overlap
    /// into a neighbouring tile's buffer are not drawn twice (which would
    /// double-blend translucent fills). Labels are not clipped.
    pub clip: Option<[f32; 4]>,
    pub features: Vec<RenderFeature>,
}

impl RenderLayer {
    pub fn new(z_order: i32) -> Self {
        Self {
            z_order,
            clip: None,
            features: Vec::new(),
        }
    }

    pub fn push(&mut self, feature: RenderFeature) {
        self.features.push(feature);
    }
}

/// One drawing primitive.
#[derive(Debug, Clone)]
pub enum RenderFeature {
    /// A filled polygon. `coords` holds the exterior followed by any holes;
    /// holes are cut out with the even-odd rule.
    Fill {
        coords: Vec<Vec<[f32; 2]>>,
        color: Color,
    },
    /// A stroked polyline. A line whose first and last points coincide is
    /// a closed ring.
    Stroke {
        coords: Vec<[f32; 2]>,
        color: Color,
        /// Width in pixels.
        width: f32,
        /// The width the style gives at the *next* integer zoom, for GPU
        /// consumers that interpolate between zoom levels. Equals `width`
        /// when the style's width does not depend on zoom.
        width_next_zoom: f32,
        cap: LineCap,
        join: LineJoin,
        /// Dash pattern as alternating on/off lengths in pixels; empty is
        /// solid.
        dash: Vec<f32>,
    },
    /// A disc, optionally outlined.
    Circle {
        center: [f32; 2],
        /// Radius in pixels (`radius_next_zoom` as for `Stroke::width_next_zoom`).
        radius: f32,
        radius_next_zoom: f32,
        color: Color,
        stroke_color: Color,
        stroke_width: f32,
    },
    /// A text label that still needs to be placed (see
    /// [`osmic_text::LabelPlacer`]).
    Label(LabelCandidate),
}
