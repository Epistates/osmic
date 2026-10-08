//! Node location storage for OSM processing.
//!
//! - [`SparseNodeIndex`]: memory proportional to the number of nodes
//!   (~8 bytes each). The default for extracts of any size.
//! - [`DenseNodeStore`]: one slot per possible id, in anonymous memory or a
//!   persistent file. Best for full-planet inputs and for replication, where
//!   the store must be updated in place and survive restarts.
//!
//! Both implement [`osmic_core::NodeLocationStore`] and store coordinates at
//! OSM's native 1e-7° precision, so lookups return exactly what the input
//! contained.

mod dense;
mod error;
mod sparse;

pub use dense::DenseNodeStore;
pub use error::NodeStoreError;
pub use sparse::{NodeRun, SealedNodeRun, SparseNodeIndex};

use osmic_core::{FixedCoord, NodeLocationStore};

/// Either kind of node store, for callers that choose at runtime.
pub enum NodeIndex {
    Sparse(SparseNodeIndex),
    Dense(DenseNodeStore),
}

impl NodeLocationStore for NodeIndex {
    #[inline]
    fn get(&self, node_id: i64) -> Option<FixedCoord> {
        match self {
            Self::Sparse(s) => s.get(node_id),
            Self::Dense(d) => d.get(node_id),
        }
    }
}
