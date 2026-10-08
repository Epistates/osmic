use thiserror::Error;

/// Replication errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReplError {
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    #[error("invalid change file: {0}")]
    Osc(String),

    #[error("{what} exceeds the {limit}-byte limit")]
    TooLarge { what: &'static str, limit: u64 },

    #[error("request to {url} failed: {message}")]
    Http { url: String, message: String },

    #[error("replication state: {0}")]
    State(String),

    #[error("OSM data")]
    Osm(#[from] osmic_osm::OsmError),
}
