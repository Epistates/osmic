use thiserror::Error;

use crate::proto::DecodeError;

/// Errors from tile generation and encoding.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TileError {
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    #[error("corrupt intermediate tile data")]
    Decode(#[from] DecodeError),

    /// A tile could not be encoded.
    #[error("failed to encode tile {tile}: {message}")]
    Encode { tile: String, message: String },

    #[error("PMTiles archive: {0}")]
    Archive(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("OSM input")]
    Osm(#[from] osmic_osm::OsmError),
}
