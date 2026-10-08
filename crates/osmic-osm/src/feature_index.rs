//! R-tree spatial index over a feature slice.

use rstar::{AABB, RTree, RTreeObject};

use osmic_core::BBox;

use crate::feature::Feature;

/// R-tree entry: a feature's position in the indexed slice and its envelope.
#[derive(Debug, Clone)]
struct SpatialEntry {
    index: usize,
    envelope: AABB<[f64; 2]>,
}

impl RTreeObject for SpatialEntry {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

/// Immutable spatial index over a feature slice, bulk-loaded in O(n log n).
///
/// Indices returned by queries refer to the slice passed to
/// [`FeatureIndex::build`].
pub struct FeatureIndex {
    tree: RTree<SpatialEntry>,
}

impl FeatureIndex {
    /// Bulk-load the bounding boxes of `features`. Features with empty
    /// geometry are not indexed.
    pub fn build(features: &[Feature]) -> Self {
        let items = features
            .iter()
            .enumerate()
            .filter_map(|(index, f)| {
                let bb = f.bbox();
                bb.is_valid().then(|| SpatialEntry {
                    index,
                    envelope: AABB::from_corners(
                        [bb.min_lon, bb.min_lat],
                        [bb.max_lon, bb.max_lat],
                    ),
                })
            })
            .collect();
        Self {
            tree: RTree::bulk_load(items),
        }
    }

    /// Indices of features whose bounding box intersects `bbox`.
    pub fn query_bbox(&self, bbox: &BBox) -> impl Iterator<Item = usize> + '_ {
        let envelope =
            AABB::from_corners([bbox.min_lon, bbox.min_lat], [bbox.max_lon, bbox.max_lat]);
        self.tree
            .locate_in_envelope_intersecting(envelope)
            .map(|entry| entry.index)
    }

    /// Number of indexed features.
    pub fn len(&self) -> usize {
        self.tree.size()
    }

    /// Whether no feature is indexed.
    pub fn is_empty(&self) -> bool {
        self.tree.size() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::{AmenityKind, FeatureKind};
    use crate::tags::Tags;
    use osmic_core::{Geometry, OsmId};

    #[test]
    fn query_returns_intersecting_features() {
        let point = |id, x, y| Feature {
            id: OsmId::node(id),
            kind: FeatureKind::Amenity(AmenityKind::Cafe),
            geometry: Geometry::Point(geo_types::Point::new(x, y)),
            tags: Tags::new(),
        };
        let features = [point(1, 0.0, 0.0), point(2, 10.0, 10.0), point(3, 0.5, 0.5)];
        let idx = FeatureIndex::build(&features);
        let mut hits: Vec<_> = idx.query_bbox(&BBox::new(-1.0, -1.0, 1.0, 1.0)).collect();
        hits.sort_unstable();
        assert_eq!(hits, [0, 2]);
        assert_eq!(idx.len(), 3);
    }
}
