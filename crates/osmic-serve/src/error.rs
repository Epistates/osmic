//! Error type for the tile server.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Errors returned while configuring, opening or running a [`TileServer`](crate::TileServer).
///
/// Per-request failures never surface as `ServeError`; they are converted to
/// HTTP responses by the handlers.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// A configuration value was rejected.
    #[error("invalid configuration for `{field}`: {reason}")]
    InvalidConfig {
        /// Name of the offending configuration field.
        field: &'static str,
        /// Human readable explanation.
        reason: String,
    },

    /// The configured PMTiles archive does not exist.
    #[error("PMTiles archive not found: {}", .0.display())]
    ArchiveNotFound(PathBuf),

    /// The PMTiles archive exists but could not be opened or read.
    #[error("failed to read PMTiles archive {}", path.display())]
    Archive {
        /// Path of the archive.
        path: PathBuf,
        /// Underlying PMTiles error.
        #[source]
        source: pmtiles::PmtError,
    },

    /// The listening socket could not be bound.
    #[error("failed to bind {addr}")]
    Bind {
        /// Address that was requested.
        addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The HTTP server terminated with an I/O error.
    #[error("server error")]
    Serve(#[source] std::io::Error),
}
