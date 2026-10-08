use rstar::{AABB, RTree, RTreeObject};

use osmic_core::BBox;
use osmic_osm::feature::Feature;

/// Entry in the spatial R-tree: stores the feature's array index and its envelope.
#[derive(Debug, Clone)]
pub struct SpatialEntry {
    pub index: usize,
    pub envelope: AABB<[f64; 2]>,
}

impl RTreeObject for SpatialEntry {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

/// Immutable spatial index over features, built using rstar bulk-loading.
///
/// Provides O(log n) bounding-box queries over millions of features.
pub struct FeatureIndex {
    tree: RTree<SpatialEntry>,
}

impl FeatureIndex {
    /// Bulk-load features into an R-tree. O(n log n).
    pub fn build(features: &[Feature]) -> Self {
        let items: Vec<SpatialEntry> = features
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let bb = f.bbox();
                SpatialEntry {
                    index: i,
                    envelope: AABB::from_corners(
                        [bb.min_lon, bb.min_lat],
                        [bb.max_lon, bb.max_lat],
                    ),
                }
            })
            .collect();

        let tree = RTree::bulk_load(items);
        Self { tree }
    }

    /// Query features intersecting the given bounding box.
    /// Returns indices into the original feature array.
    pub fn query_bbox(&self, bbox: &BBox) -> Vec<usize> {
        let envelope =
            AABB::from_corners([bbox.min_lon, bbox.min_lat], [bbox.max_lon, bbox.max_lat]);
        self.tree
            .locate_in_envelope_intersecting(&envelope)
            .map(|entry| entry.index)
            .collect()
    }

    /// Number of entries in the index.
    pub fn len(&self) -> usize {
        self.tree.size()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.size() == 0
    }
}
