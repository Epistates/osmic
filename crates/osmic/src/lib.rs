// Osmic facade crate: single-dependency access to the full SDK.
//
// ```toml
// [dependencies]
// osmic = "0.1"
// ```

pub use osmic_app as app;
pub use osmic_core as core;
pub use osmic_geo as geo;
pub use osmic_index as index;
pub use osmic_osm as osm;
pub use osmic_render as render;
pub use osmic_serve as serve;
pub use osmic_style as style;
pub use osmic_text as text;
pub use osmic_tiles as tiles;

/// Common types re-exported for convenience.
pub mod prelude {
    // Core types
    pub use osmic_core::{
        BBox, Color, FixedCoord, Geometry, LonLat, OsmId, OsmType, OsmicError, OsmicResult,
        TileCoord, Zoom,
    };

    // OSM data model
    pub use osmic_osm::geojson::load_geojson;
    pub use osmic_osm::{
        Feature, FeatureIndex, FeatureKind, FeatureSink, LayerSet, PbfProcessor, PipelineConfig,
        TagFilter, TagStore, Tags,
    };

    // Node locations
    pub use osmic_index::{DenseNodeStore, SparseNodeIndex};

    // App framework
    pub use osmic_app::{App, Plugin, PluginGroup};

    // Tile generation
    pub use osmic_tiles::{MvtEncoder, PmTilesArchive, TileGenerator, TileGeneratorConfig};

    // Rendering
    pub use osmic_render::backend::{RenderBackend, RenderConfig};
    pub use osmic_render::skia::SkiaBackend;

    // Style
    pub use osmic_style::default_style_json;

    // Server
    pub use osmic_serve::{TileServer, TileServerConfig, TileServerPlugin};
}

/// Plugin group that includes all headless (non-GPU) plugins.
pub struct HeadlessPlugins;

impl osmic_app::PluginGroup for HeadlessPlugins {
    fn build(self) -> osmic_app::PluginGroupBuilder {
        osmic_app::PluginGroupBuilder::new()
    }
}

/// Plugin group that includes all default plugins for interactive map rendering.
pub struct DefaultPlugins;

impl osmic_app::PluginGroup for DefaultPlugins {
    fn build(self) -> osmic_app::PluginGroupBuilder {
        osmic_app::PluginGroupBuilder::new()
    }
}
