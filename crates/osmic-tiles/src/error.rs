use thiserror::Error;

use crate::proto::DecodeError;

/// Errors from tile generation and encoding.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TileError {
    /// Reading or writing a file failed.
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    /// Data the generator wrote to its temporary files read back corrupt.
    #[error("corrupt intermediate tile data")]
    Decode(#[from] DecodeError),

    /// A tile could not be encoded.
    #[error("failed to encode tile {tile}: {message}")]
    Encode {
        /// The tile (or layer) being encoded.
        tile: String,
        /// Why encoding failed.
        message: String,
    },

    /// Creating or writing a PMTiles archive failed.
    #[error("PMTiles archive: {0}")]
    Archive(String),

    /// The settings are invalid.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// Reading the OSM input failed.
    #[error("OSM input")]
    Osm(#[from] osmic_osm::OsmError),
}
