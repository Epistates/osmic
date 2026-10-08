//! osmic: single-dependency access to the full SDK.
//!
//! Each workspace crate is re-exported as a module ([`osm`], [`tiles`],
//! [`serve`], …) and the most common types are collected in [`prelude`].
//! Heavier parts are opt-in features: `extract` (`osmic::extract`),
//! `replication` (`osmic::repl`), `accel` (`osmic::accel`, Apple Silicon)
//! and `mlt` (MapLibre Tile output).
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

#[cfg(feature = "accel")]
pub use osmic_accel as accel;
#[cfg(feature = "extract")]
pub use osmic_extract as extract;
#[cfg(feature = "replication")]
pub use osmic_repl as repl;

/// Common types re-exported for convenience.
pub mod prelude {
    // Core types
    pub use osmic_core::{
        BBox, Color, FixedCoord, Geometry, LonLat, OsmId, OsmType, TileCoord, Zoom,
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

    // Tile generation. `TileRenderConfig` is the type of
    // `TileGeneratorConfig::render` (`osmic::tiles::RenderConfig`).
    pub use osmic_tiles::RenderConfig as TileRenderConfig;
    pub use osmic_tiles::{MvtEncoder, PmTilesArchive, TileGenerator, TileGeneratorConfig};

    // Rendering. The raster backend's settings are
    // `osmic::render::RenderConfig`, deliberately not in the prelude.
    pub use osmic_render::backend::RenderBackend;
    pub use osmic_render::skia::SkiaBackend;

    // Style
    pub use osmic_style::default_style_json;

    // Server
    pub use osmic_serve::{ServerRoutes, TileServer, TileServerConfig, TileServerPlugin};
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;

    #[test]
    fn prelude_render_config_is_the_tile_generators() {
        let mut config = TileGeneratorConfig::default();
        config.render = TileRenderConfig::default();
        assert!(config.max_tile_bytes > 0);
        // The raster backend's settings stay reachable through the module.
        let _ = crate::render::RenderConfig::default();
    }
}

/// The README's examples, compiled as doctests so they stay correct.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
pub struct ReadmeDoctests;
