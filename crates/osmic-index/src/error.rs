use thiserror::Error;

/// Errors from node location stores.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NodeStoreError {
    #[error("node store I/O error")]
    Io(#[from] std::io::Error),

    /// A node id is negative or beyond the store's capacity. Raised instead
    /// of silently dropping the node.
    #[error("node id {id} is outside the store's capacity of {capacity} slots")]
    IdOutOfRange { id: i64, capacity: usize },

    #[error("invalid node store capacity for max node id {max_node_id}")]
    InvalidCapacity { max_node_id: i64 },

    #[error("invalid node store file: {0}")]
    InvalidFile(String),
}

impl From<NodeStoreError> for osmic_core::OsmicError {
    fn from(e: NodeStoreError) -> Self {
        match e {
            NodeStoreError::Io(io) => Self::Io(io),
            other => Self::Index(other.to_string()),
        }
    }
}
