use thiserror::Error;

/// Errors from node location stores.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NodeStoreError {
    /// Opening, sizing, mapping or flushing the backing memory or file
    /// failed.
    #[error("node store I/O error")]
    Io(#[from] std::io::Error),

    /// A node id is negative or beyond the store's capacity. Raised instead
    /// of silently dropping the node.
    #[error("node id {id} is outside the store's capacity of {capacity} slots")]
    IdOutOfRange {
        /// The rejected node id.
        id: i64,
        /// Number of slots in the store; valid ids are `0..capacity`.
        capacity: usize,
    },

    /// The requested maximum node id is negative, or the store it implies
    /// is larger in bytes than `usize` can express.
    #[error("invalid node store capacity for max node id {max_node_id}")]
    InvalidCapacity {
        /// The requested maximum node id.
        max_node_id: i64,
    },

    /// A file is not a valid node store: too short, wrong magic or format
    /// version, or a length that disagrees with its header. The message
    /// says which.
    #[error("invalid node store file: {0}")]
    InvalidFile(String),

    /// Another store (in this or another process) has the file open.
    #[error("node store {} is in use by another process", .0.display())]
    Locked(std::path::PathBuf),
}
