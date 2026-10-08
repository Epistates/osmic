//! Style-driven map rendering.
//!
//! ```text
//! osmic_style::Style ──┐
//!                      ├─ SceneBuilder ─▶ SceneGraph ─┬─ SkiaBackend (tiny-skia + osmic-text)  ─▶ RGBA / PNG
//! decoded tile features┘                              └─ tessellate_scene (lyon) ─▶ GPU Mesh
//! ```
//!
//! * [`SceneBuilder`] evaluates a [`osmic_style::Style`] (filters,
//!   data-driven and zoom-dependent properties) against decoded vector-tile
//!   features and a [`PixelMapping`] and produces a backend-agnostic
//!   [`SceneGraph`]. There is no other place where the palette lives.
//! * [`SkiaBackend`] rasterises a scene in software, including dashes,
//!   even-odd fills, and priority-placed, collision-free labels (via
//!   [`osmic_text`]).
//! * [`tessellate_scene`] turns a scene into lyon triangle meshes whose
//!   strokes are extruded in screen space by the vertex shader, for GPU
//!   consumers such as the viewer.
//! * [`Camera`] is the Web Mercator camera shared by interactive and static
//!   rendering.

pub mod backend;
pub mod camera;
mod error;
pub mod scene;
pub mod scene_builder;
pub mod skia;
pub mod tessellate;

pub use backend::{RenderBackend, RenderConfig};
pub use camera::{
    Camera, MAX_TILE_ZOOM, MAX_VISIBLE_TILES, MAX_ZOOM, MIN_ZOOM, PixelMapping, TILE_SIZE,
    TileTransform, VisibleTile,
};
pub use error::{RenderError, RenderResult};
pub use scene::{RenderFeature, RenderLayer, SceneGraph};
pub use scene_builder::{SceneBuilder, SceneOptions, build_scene};
pub use skia::SkiaBackend;
pub use tessellate::{
    MAX_DASHES_PER_LINE, MIN_DASH_PERIOD, Mesh, MeshVertex, TessellationOptions, dash_polyline,
    tessellate_scene,
};
