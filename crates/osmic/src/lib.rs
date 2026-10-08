//! osmic: single-dependency access to the full SDK.
//!
//! Each workspace crate is re-exported as a module ([`osm`], [`tiles`],
//! [`serve`], …) and the most common types are collected in [`prelude`].
//!
//! ```no_run
//! use osmic::prelude::*;
//!
//! // Serve a PMTiles archive until SIGINT/SIGTERM.
//! App::new()
//!     .add_plugin(TileServerPlugin::new("tiles.pmtiles"))
//!     .run()?;
//! # Ok::<(), AppError>(())
//! ```

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
    pub use osmic_app::{App, AppError, BoxError, Plugin, PluginGroup};

    // Tile generation
    pub use osmic_tiles::{MvtEncoder, PmTilesArchive, TileGenerator, TileGeneratorConfig};

    // Rendering
    pub use osmic_render::backend::{RenderBackend, RenderConfig};
    pub use osmic_render::skia::SkiaBackend;

    // Style
    pub use osmic_style::default_style_json;

    // Server
    pub use osmic_serve::{ServerRoutes, TileServer, TileServerConfig, TileServerPlugin};
}
