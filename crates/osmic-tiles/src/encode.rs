//! Tile encoders.

use crate::error::TileError;
use crate::model::TileLayer;

/// Vector tile format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TileFormat {
    /// Mapbox Vector Tile 2.1.
    Mvt,
    /// MapLibre Tile (requires the `mlt` feature and Rust 1.98).
    Mlt,
}

impl TileFormat {
    /// MIME type of tiles in this format.
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Mvt => "application/vnd.mapbox-vector-tile",
            Self::Mlt => "application/vnd.maplibre-vector-tile",
        }
    }

    /// TileJSON / PMTiles metadata `format` value.
    pub const fn metadata_format(self) -> &'static str {
        match self {
            Self::Mvt => "pbf",
            Self::Mlt => "mlt",
        }
    }
}

/// Encodes one tile's layers into bytes (uncompressed).
pub trait TileEncoder: Send + Sync {
    /// The format this encoder produces.
    fn format(&self) -> TileFormat;

    /// Encode `layers`. Returns an empty vector if there is nothing to
    /// encode.
    fn encode(&self, layers: &[TileLayer]) -> Result<Vec<u8>, TileError>;
}

/// Mapbox Vector Tile encoder.
#[derive(Debug, Clone, Copy, Default)]
pub struct MvtEncoder;

impl TileEncoder for MvtEncoder {
    fn format(&self) -> TileFormat {
        TileFormat::Mvt
    }

    fn encode(&self, layers: &[TileLayer]) -> Result<Vec<u8>, TileError> {
        Ok(crate::mvt::encode_tile(layers))
    }
}
