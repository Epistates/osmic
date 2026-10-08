use thiserror::Error;

/// Replication errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReplError {
    /// Reading the input or writing the output failed.
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    /// A change file is malformed, structurally invalid, or exceeds an
    /// [`OscLimits`](crate::OscLimits) count or length limit.
    #[error("invalid change file: {0}")]
    Osc(String),

    /// A download or decompressed change file exceeds its size limit.
    #[error("{what} exceeds the {limit}-byte limit")]
    TooLarge {
        /// What was too large, e.g. `"download"`.
        what: &'static str,
        /// The limit, in bytes.
        limit: u64,
    },

    /// A request failed: a non-retryable HTTP status, a missing
    /// `state.txt`, or a transient failure that persisted through every
    /// attempt.
    #[error("request to {url} failed: {message}")]
    Http {
        /// The URL requested.
        url: String,
        /// The status or transport error.
        message: String,
    },

    /// The replication state is missing, invalid or inconsistent, or the
    /// requested operation does not fit it: no URL or sequence to start
    /// from, a mismatched stream, an unsorted input, a non-HTTPS URL.
    #[error("replication state: {0}")]
    State(String),

    /// Reading or writing the PBF failed.
    #[error("OSM data")]
    Osm(#[from] osmic_osm::OsmError),
}
