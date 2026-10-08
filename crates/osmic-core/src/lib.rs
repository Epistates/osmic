//! Shared types for osmic: coordinates, typed OSM ids, geometry, bounding
//! boxes, tile coordinates, Web Mercator projection, clipping,
//! simplification and errors.

pub mod bbox;
pub mod clip;
pub mod color;
pub mod coord;
pub mod error;
pub mod fs;
pub mod geometry;
pub mod mercator;
pub mod osm_id;
pub mod simplify;
pub mod tile;

pub use bbox::BBox;
pub use color::Color;
pub use coord::{FixedCoord, LonLat};
pub use error::{OsmicError, OsmicResult};
pub use geometry::{Geometry, GeometryType};
pub use osm_id::{OsmId, OsmType};
pub use tile::{TileCoord, Zoom};

/// Read access to node locations, keyed by node id.
///
/// Implementations live in `osmic-index`; pipelines only need lookups.
pub trait NodeLocationStore: Send + Sync {
    /// The location of `node_id`, or `None` if the store has no such node.
    fn get(&self, node_id: i64) -> Option<FixedCoord>;
}
