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
    /// The layers; drawn in ascending [`RenderLayer::z_order`].
    pub layers: Vec<RenderLayer>,
}

impl SceneGraph {
    /// An empty scene cleared to `background`.
    pub fn new(background: Color) -> Self {
        Self {
            background,
            layers: Vec::new(),
        }
    }

    /// Add a layer.
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

/// The primitives of one style layer.
#[derive(Debug, Clone)]
pub struct RenderLayer {
    /// Layers draw in ascending `z_order` (ties keep their order); for
    /// scenes built from a style it is the layer's index in the style.
    pub z_order: i32,
    /// Rectangle `[x0, y0, x1, y1]` the layer's geometry is clipped to.
    /// Per-tile scenes set it to the tile bounds so features that overlap
    /// into a neighbouring tile's buffer are not drawn twice (which would
    /// double-blend translucent fills). Labels are not clipped. The GPU
    /// tessellation ignores it.
    pub clip: Option<[f32; 4]>,
    /// The primitives, in drawing order.
    pub features: Vec<RenderFeature>,
}

impl RenderLayer {
    /// An empty, unclipped layer at `z_order`.
    pub fn new(z_order: i32) -> Self {
        Self {
            z_order,
            clip: None,
            features: Vec::new(),
        }
    }

    /// Add a primitive.
    pub fn push(&mut self, feature: RenderFeature) {
        self.features.push(feature);
    }
}

/// One drawing primitive.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RenderFeature {
    /// A filled polygon. `coords` holds the exterior followed by any holes;
    /// holes are cut out with the even-odd rule.
    Fill {
        /// Exterior ring, then holes; rings are implicitly closed.
        coords: Vec<Vec<[f32; 2]>>,
        /// Fill color (straight alpha).
        color: Color,
    },
    /// A stroked polyline. A line whose first and last points coincide is
    /// a closed ring.
    Stroke {
        /// The polyline.
        coords: Vec<[f32; 2]>,
        /// Line color (straight alpha).
        color: Color,
        /// Width in pixels.
        width: f32,
        /// The width the style gives at the *next* integer zoom, for GPU
        /// consumers that interpolate between zoom levels. Equals `width`
        /// when the style's width does not depend on zoom.
        width_next_zoom: f32,
        /// End caps.
        cap: LineCap,
        /// Joins between segments.
        join: LineJoin,
        /// Dash pattern as alternating on/off lengths in pixels; empty is
        /// solid. Patterns that cannot be drawn (see
        /// [`crate::MIN_DASH_PERIOD`]) are solid.
        dash: Vec<f32>,
    },
    /// A disc with an optional stroke, which MapLibre draws as a ring
    /// outside the radius (from `radius` to `radius + stroke_width`).
    Circle {
        /// Centre in pixels.
        center: [f32; 2],
        /// Radius in pixels (`radius_next_zoom` as for `Stroke::width_next_zoom`).
        radius: f32,
        /// The radius at the next integer zoom.
        radius_next_zoom: f32,
        /// Fill color (straight alpha).
        color: Color,
        /// Stroke color (straight alpha).
        stroke_color: Color,
        /// Stroke width in pixels; 0 for none.
        stroke_width: f32,
    },
    /// A text label that still needs to be placed (see
    /// [`osmic_text::LabelPlacer`]).
    Label(LabelCandidate),
}
