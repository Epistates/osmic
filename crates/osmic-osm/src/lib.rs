//! OpenStreetMap data model and processing for osmic.
//!
//! - [`classify`]: tags → layers and area semantics.
//! - [`feature`]: classified features with typed ids and geometry.
//! - [`multipolygon`]: area assembly for multipolygon/boundary relations.
//! - [`pipeline`] (`native`): parallel two-pass PBF processing.
//! - [`pbf`] (`native`): PBF header inspection, block decoding, writing.
//! - [`geojson`] (`native`): streaming GeoJSON input.

pub mod classify;
mod error;
pub mod feature;
pub mod feature_index;
pub mod filter;
#[cfg(feature = "native")]
pub mod geojson;
pub mod layers;
pub mod multipolygon;
#[cfg(feature = "native")]
pub mod pbf;
#[cfg(feature = "native")]
pub mod pipeline;
pub mod tags;

pub use error::OsmError;
pub use feature::{Feature, FeatureKind};
pub use feature_index::FeatureIndex;
pub use filter::TagFilter;
pub use layers::{Layer, LayerSet};
#[cfg(feature = "native")]
pub use pipeline::{
    CollectSink, FeatureSink, IncompleteWays, NodeScan, NodeStorage, PbfProcessor, PipelineConfig,
    PipelineStats, ProcessedData, RelationMember, RelationRecord, RunOutput, scan_nodes,
};
pub use tags::{TagRetention, TagStore, Tags, WellKnownKey};
