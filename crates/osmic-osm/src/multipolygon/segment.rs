//! Segment-oriented decomposition of multipolygon relation members.
//!
//! Each member way is split into directed edges (`NodeRefSegment`) between
//! consecutive nodes. Endpoints are quantized to nanodegrees (`i32`) for
//! exact-equality hashing — OSM nodes are stored with 1e-7 degree precision
//! in the PBF so byte-identical shared nodes produce identical quant keys.
//!
//! A `HashMap<QuantKey, SmallVec<[usize; 2]>>` serves as the endpoint index
//! for O(1) lookup of segments sharing a point. Most endpoints are touched
//! by exactly two segments (a way's internal node), so the `SmallVec<[_; 2]>`
//! inline capacity avoids heap allocation on the common path.

use geo_types::Coord;
use smallvec::SmallVec;
use std::collections::HashMap;

/// Role of a segment — inherited from the parent way's relation membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Outer,
    Inner,
}

/// A directed edge between two consecutive nodes of a way, tagged with the
/// way id and role for diagnostics and role-aware ring closing.
#[derive(Debug, Clone, Copy)]
pub struct NodeRefSegment {
    pub start: Coord<f64>,
    pub end: Coord<f64>,
    pub way_id: i64,
    pub role: Role,
    pub used: bool,
}

/// Quantized coordinate key. 1 ulp at nanodegrees ≈ 1.1 cm at equator,
/// more than enough to match identical OSM node references.
pub type QuantKey = (i32, i32);

#[inline]
pub fn quantize(c: Coord<f64>) -> QuantKey {
    ((c.x * 1e7).round() as i32, (c.y * 1e7).round() as i32)
}

/// Flat segment soup with an endpoint index for ring-closing lookups.
pub struct SegmentList {
    segments: Vec<NodeRefSegment>,
    /// For each quantized endpoint, the list of segment indices that touch
    /// it at either end. Ring closing filters by `used` and `role`.
    endpoint_index: HashMap<QuantKey, SmallVec<[usize; 2]>>,
}

impl SegmentList {
    /// Build a segment list from the outer and inner member ways of a
    /// multipolygon relation. `way_geoms` must already contain resolved
    /// coordinate sequences for the referenced way ids; missing ways are
    /// silently skipped (the assembler's caller is responsible for the
    /// higher-level "not enough data" report).
    pub fn from_ways(
        outer_way_ids: &[i64],
        inner_way_ids: &[i64],
        way_geoms: &HashMap<i64, Vec<Coord<f64>>>,
    ) -> Self {
        let mut segments = Vec::new();
        let mut endpoint_index: HashMap<QuantKey, SmallVec<[usize; 2]>> = HashMap::new();

        let push_way = |way_ids: &[i64],
                        role: Role,
                        segments: &mut Vec<NodeRefSegment>,
                        idx: &mut HashMap<QuantKey, SmallVec<[usize; 2]>>| {
            for &way_id in way_ids {
                let Some(coords) = way_geoms.get(&way_id) else {
                    continue;
                };
                if coords.len() < 2 {
                    continue;
                }
                for window in coords.windows(2) {
                    let start = window[0];
                    let end = window[1];
                    // Skip zero-length segments — these come from duplicate
                    // node refs which occur occasionally in broken data.
                    if quantize(start) == quantize(end) {
                        continue;
                    }
                    let seg_idx = segments.len();
                    segments.push(NodeRefSegment {
                        start,
                        end,
                        way_id,
                        role,
                        used: false,
                    });
                    idx.entry(quantize(start)).or_default().push(seg_idx);
                    idx.entry(quantize(end)).or_default().push(seg_idx);
                }
            }
        };

        push_way(
            outer_way_ids,
            Role::Outer,
            &mut segments,
            &mut endpoint_index,
        );
        push_way(
            inner_way_ids,
            Role::Inner,
            &mut segments,
            &mut endpoint_index,
        );

        Self {
            segments,
            endpoint_index,
        }
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&NodeRefSegment> {
        self.segments.get(idx)
    }

    /// Find the first unused segment in the list (any role). Used as the
    /// starting point for a new ring.
    pub fn find_first_unused(&self) -> Option<usize> {
        self.segments.iter().position(|s| !s.used)
    }

    /// Find an unused segment whose start or end matches `point` and whose
    /// role matches `role`. Returns `Some((segment_idx, needs_reverse))`
    /// where `needs_reverse` is true if the segment must be flipped so that
    /// its start aligns with `point`.
    ///
    /// **Preference:** non-reversing matches (segment.start == point) are
    /// preferred over reversing matches (segment.end == point). This avoids
    /// unnecessary flips and produces more predictable ring orientations
    /// when the input data is already well-ordered.
    pub fn find_matching_unused(&self, point: Coord<f64>, role: Role) -> Option<(usize, bool)> {
        let key = quantize(point);
        let bucket = self.endpoint_index.get(&key)?;

        // First pass: prefer segments whose start matches (no reversal).
        for &seg_idx in bucket {
            let seg = &self.segments[seg_idx];
            if seg.used || seg.role != role {
                continue;
            }
            if quantize(seg.start) == key {
                return Some((seg_idx, false));
            }
        }
        // Second pass: fall back to segments whose end matches (needs reversal).
        for &seg_idx in bucket {
            let seg = &self.segments[seg_idx];
            if seg.used || seg.role != role {
                continue;
            }
            if quantize(seg.end) == key {
                return Some((seg_idx, true));
            }
        }
        None
    }

    pub fn mark_used(&mut self, idx: usize) {
        self.segments[idx].used = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord(x: f64, y: f64) -> Coord<f64> {
        Coord { x, y }
    }

    #[test]
    fn quantize_produces_stable_keys() {
        // Same numeric coord → same key
        let a = coord(1.23456789, 2.34567891);
        let b = coord(1.23456789, 2.34567891);
        assert_eq!(quantize(a), quantize(b));

        // Different coord → different key
        let c = coord(1.23456780, 2.34567891);
        assert_ne!(quantize(a), quantize(c));
    }

    #[test]
    fn from_ways_builds_endpoint_index() {
        // A simple square: way_1 = [(0,0), (1,0), (1,1)], way_2 = [(1,1), (0,1), (0,0)]
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, vec![coord(0.0, 0.0), coord(1.0, 0.0), coord(1.0, 1.0)]);
        way_geoms.insert(2, vec![coord(1.0, 1.0), coord(0.0, 1.0), coord(0.0, 0.0)]);

        let list = SegmentList::from_ways(&[1, 2], &[], &way_geoms);
        // 2 segments per way × 2 ways = 4 segments
        assert_eq!(list.len(), 4);

        // Shared endpoint (1,1) should appear in 2 segments (end of way_1's
        // second segment and start of way_2's first segment).
        let shared = list.endpoint_index.get(&quantize(coord(1.0, 1.0))).unwrap();
        assert_eq!(shared.len(), 2, "shared endpoint must be in 2 buckets");
    }

    #[test]
    fn find_matching_unused_by_start() {
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, vec![coord(0.0, 0.0), coord(1.0, 0.0)]);
        way_geoms.insert(2, vec![coord(1.0, 0.0), coord(1.0, 1.0)]);

        let list = SegmentList::from_ways(&[1, 2], &[], &way_geoms);
        // Starting from (1.0, 0.0) we should find the second way's first segment
        let (idx, needs_reverse) = list
            .find_matching_unused(coord(1.0, 0.0), Role::Outer)
            .expect("match must be found");
        let seg = list.get(idx).unwrap();
        assert_eq!(seg.way_id, 2);
        assert!(
            !needs_reverse,
            "segment start already equals the query point"
        );
    }

    #[test]
    fn find_matching_unused_by_end_returns_reverse_flag() {
        let mut way_geoms = HashMap::new();
        // way 1 starts at (1,0) so its start matches
        way_geoms.insert(1, vec![coord(0.0, 0.0), coord(1.0, 0.0)]);

        let list = SegmentList::from_ways(&[1], &[], &way_geoms);
        // Query by the END of the segment — should need reversal
        let (idx, needs_reverse) = list
            .find_matching_unused(coord(1.0, 0.0), Role::Outer)
            .expect("match must be found");
        let seg = list.get(idx).unwrap();
        assert_eq!(seg.start, coord(0.0, 0.0));
        assert!(needs_reverse, "querying by end point must flag reversal");
    }

    #[test]
    fn zero_length_segments_are_dropped() {
        let mut way_geoms = HashMap::new();
        // Duplicate node — the second pair is zero-length.
        way_geoms.insert(1, vec![coord(0.0, 0.0), coord(0.0, 0.0), coord(1.0, 0.0)]);
        let list = SegmentList::from_ways(&[1], &[], &way_geoms);
        // Only one non-degenerate segment: (0,0) -> (1,0)
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn role_filter_excludes_other_role() {
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, vec![coord(0.0, 0.0), coord(1.0, 0.0)]);
        let list = SegmentList::from_ways(&[], &[1], &way_geoms); // inner only
        // Asking for an outer segment should return None
        assert!(
            list.find_matching_unused(coord(0.0, 0.0), Role::Outer)
                .is_none()
        );
        // Asking for an inner segment should return Some
        assert!(
            list.find_matching_unused(coord(0.0, 0.0), Role::Inner)
                .is_some()
        );
    }
}
