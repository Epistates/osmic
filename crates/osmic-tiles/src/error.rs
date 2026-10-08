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

impl From<TileError> for osmic_core::OsmicError {
    fn from(e: TileError) -> Self {
        match e {
            TileError::Io(io) => Self::Io(io),
            other => {
                let mut msg = other.to_string();
                let mut source = std::error::Error::source(&other);
                while let Some(s) = source {
                    msg.push_str(": ");
                    msg.push_str(&s.to_string());
                    source = s.source();
                }
                Self::Tile(msg)
            }
        }
    }
}
