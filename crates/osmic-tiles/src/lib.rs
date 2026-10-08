#![warn(missing_docs)]
//! Vector tile generation for osmic.
//!
//! - [`render`]: features → per-zoom, per-tile geometry (projection,
//!   simplification, clipping, quantisation).
//! - [`model`]: the in-memory tile representation.
//! - [`mvt`], [`mlt`] (`mlt` feature): tile encoders; [`mvt_decode`]: a
//!   decoder that is safe on untrusted input.
//! - [`assemble`]: size-budgeted tile assembly and compression.
//! - [`pipeline`] (`native`): [`TileGenerator`], a streaming,
//!   memory-bounded generator built on a parallel external sort.
//! - [`pmtiles`] (`native`): clustered, atomically written PMTiles archives.

pub mod assemble;
pub mod encode;
mod error;
#[cfg(feature = "mlt")]
pub mod mlt;
pub mod model;
pub mod mvt;
pub mod mvt_decode;
#[cfg(feature = "native")]
pub mod pipeline;
#[cfg(feature = "native")]
pub mod pmtiles;
mod proto;
#[cfg(feature = "reader")]
pub mod reader;
#[cfg(feature = "native")]
mod record;
pub mod render;
#[cfg(feature = "native")]
mod sorter;

pub use assemble::TileCompression;
pub use encode::{MvtEncoder, TileEncoder, TileFormat};
pub use error::TileError;
#[cfg(feature = "mlt")]
pub use mlt::MltEncoder;
pub use model::{GeomType, TileFeature, TileLayer};
#[cfg(feature = "native")]
pub use pipeline::{TileGenerator, TileGeneratorConfig, TileSummary};
#[cfg(feature = "native")]
pub use pmtiles::{ArchiveInfo, PmTilesArchive};
pub use render::{AttributeMode, RenderConfig};
