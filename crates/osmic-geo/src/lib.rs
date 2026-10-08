//! Geometry algorithms for osmic: projection (re-exported from
//! `osmic-core`), simplification and ring orientation.

pub mod orient;
pub mod projection;
pub mod simplify;

pub use orient::{orient_geometry, orient_multipolygon, orient_polygon};
pub use simplify::simplify_geometry;
