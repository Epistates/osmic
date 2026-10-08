//! Sparse node location index for extracts of any size.
//!
//! A dense array costs 8 bytes per *possible* id (≈ 100 GB for today's id
//! range) no matter how few nodes an extract holds, because node ids are
//! spread across the whole range. This index costs ~8 bytes per *present*
//! node plus a small id index, and keeps O(1)-ish lookups.
//!
//! # Layout
//!
//! Nodes arrive in runs (one per PBF block). Each run is sorted, deduplicated
//! and split into [`Segment`]s covering a contiguous id span. A segment stores
//! its coordinates densely in id order plus the cheapest of three id indexes:
//!
//! - **contiguous** — the span has no gaps: no index at all (typical for
//!   planet files);
//! - **bitmap** — one bit per id in the span plus a running popcount per
//!   64-bit word (12 bytes per 64 ids);
//! - **offsets** — sorted `u32` offsets from the first id (4 bytes per node).
//!
//! Segments are kept sorted by first id. A bucket table over the id space
//! narrows a lookup to the handful of segments that can contain the id, then
//! a short binary search picks the segment.
//!
//! # Duplicates and ordering
//!
//! Runs must be supplied in input order. If two runs contain the same id (an
//! unsorted or concatenated file), the later run wins, matching "last write
//! wins" semantics of a dense store filled sequentially.

use osmic_core::{FixedCoord, NodeLocationStore};

/// Upper bound on nodes per segment; keeps merges and offsets small.
const MAX_SEGMENT_ENTRIES: usize = 1 << 16;
/// Minimum bucket width as a power of two (65 536 ids per bucket).
const MIN_BUCKET_SHIFT: u32 = 16;
/// Cap on the bucket table length (entries are `u32`, so ≤ 16 MiB).
const MAX_BUCKETS: u64 = 1 << 22;

#[derive(Debug)]
enum SegmentIndex {
    Contiguous,
    Bitmap {
        words: Box<[u64]>,
        ranks: Box<[u32]>,
    },
    Offsets(Box<[u32]>),
}

/// A sorted, gap-tolerant run of node locations covering
/// `first_id..=last_id`.
#[derive(Debug)]
struct Segment {
    first_id: i64,
    last_id: i64,
    coords: Box<[u64]>,
    index: SegmentIndex,
}

impl Segment {
    /// Build from strictly increasing ids whose span fits in `u32`.
    fn from_sorted(entries: &[(i64, u64)]) -> Self {
        debug_assert!(!entries.is_empty());
        debug_assert!(entries.windows(2).all(|w| w[0].0 < w[1].0));
        let first_id = entries[0].0;
        let last_id = entries[entries.len() - 1].0;
        let n = entries.len();
        let span = (last_id - first_id) as u64 + 1;
        let coords: Box<[u64]> = entries.iter().map(|&(_, c)| c).collect();

        let bitmap_bytes = span.div_ceil(64) * 12;
        let offsets_bytes = n as u64 * 4;
        let index = if span == n as u64 {
            SegmentIndex::Contiguous
        } else if bitmap_bytes <= offsets_bytes {
            let word_count = span.div_ceil(64) as usize;
            let mut words = vec![0u64; word_count];
            for &(id, _) in entries {
                let off = (id - first_id) as u64;
                words[(off >> 6) as usize] |= 1 << (off & 63);
            }
            let mut ranks = Vec::with_capacity(word_count);
            let mut running = 0u32;
            for w in &words {
                ranks.push(running);
                running += w.count_ones();
            }
            SegmentIndex::Bitmap {
                words: words.into_boxed_slice(),
                ranks: ranks.into_boxed_slice(),
            }
        } else {
            SegmentIndex::Offsets(
                entries
                    .iter()
                    .map(|&(id, _)| (id - first_id) as u32)
                    .collect(),
            )
        };
        Self {
            first_id,
            last_id,
            coords,
            index,
        }
    }

    #[inline]
    fn lookup(&self, id: i64) -> Option<u64> {
        if id < self.first_id || id > self.last_id {
            return None;
        }
        let off = (id - self.first_id) as u64;
        match &self.index {
            SegmentIndex::Contiguous => self.coords.get(off as usize).copied(),
            SegmentIndex::Bitmap { words, ranks } => {
                let w = (off >> 6) as usize;
                let bit = off & 63;
                let word = *words.get(w)?;
                if (word >> bit) & 1 == 0 {
                    return None;
                }
                let rank = *ranks.get(w)? + (word & ((1u64 << bit) - 1)).count_ones();
                self.coords.get(rank as usize).copied()
            }
            SegmentIndex::Offsets(offsets) => offsets
                .binary_search(&(off as u32))
                .ok()
                .and_then(|i| self.coords.get(i).copied()),
        }
    }

    fn entries(&self) -> Vec<(i64, u64)> {
        let ids: Vec<i64> = match &self.index {
            SegmentIndex::Contiguous => (self.first_id..=self.last_id).collect(),
            SegmentIndex::Bitmap { words, .. } => words
                .iter()
                .enumerate()
                .flat_map(|(w, &word)| {
                    (0..64u64)
                        .filter(move |b| (word >> b) & 1 == 1)
                        .map(move |b| self.first_id + ((w as u64) << 6 | b) as i64)
                })
                .collect(),
            SegmentIndex::Offsets(offsets) => offsets
                .iter()
                .map(|&o| self.first_id + i64::from(o))
                .collect(),
        };
        ids.into_iter().zip(self.coords.iter().copied()).collect()
    }

    fn heap_bytes(&self) -> usize {
        self.coords.len() * 8
            + match &self.index {
                SegmentIndex::Contiguous => 0,
                SegmentIndex::Bitmap { words, ranks } => words.len() * 8 + ranks.len() * 4,
                SegmentIndex::Offsets(o) => o.len() * 4,
            }
    }
}

/// Sort (stably), deduplicate keeping the *last* occurrence of each id, and
/// cut into segments.
fn segments_from_entries(entries: &mut Vec<(i64, u64)>) -> Vec<Segment> {
    if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
        // Stable sort keeps input order among equal ids; reverse-dedup then
        // keeps the last one.
        entries.sort_by_key(|&(id, _)| id);
        entries.reverse();
        entries.dedup_by_key(|&mut (id, _)| id);
        entries.reverse();
    }
    let mut segments = Vec::new();
    let mut start = 0;
    for i in 1..=entries.len() {
        let cut = i == entries.len()
            || i - start >= MAX_SEGMENT_ENTRIES
            || entries[i].0.abs_diff(entries[start].0) >= u64::from(u32::MAX);
        if cut {
            segments.push(Segment::from_sorted(&entries[start..i]));
            start = i;
        }
    }
    segments
}

/// Node locations decoded from one input block, in input order.
///
/// Fill with [`NodeRun::push`], then [`NodeRun::seal`] it (cheap to do in
/// the decoding worker) and pass the sealed runs, in input order, to
/// [`SparseNodeIndex::from_runs`].
#[derive(Debug, Default)]
pub struct NodeRun {
    entries: Vec<(i64, u64)>,
}

impl NodeRun {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, id: i64, coord: FixedCoord) {
        self.entries.push((id, coord.pack()));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Compress into the index's segment representation.
    pub fn seal(mut self) -> SealedNodeRun {
        SealedNodeRun {
            segments: segments_from_entries(&mut self.entries),
        }
    }
}

/// A [`NodeRun`] compressed into index segments.
#[derive(Debug, Default)]
pub struct SealedNodeRun {
    segments: Vec<Segment>,
}

/// Read-only sparse node location index. See the module docs.
#[derive(Debug, Default)]
pub struct SparseNodeIndex {
    first_ids: Vec<i64>,
    segments: Vec<Segment>,
    /// `buckets[b]` = index of the first segment with
    /// `last_id >= b << bucket_shift`; one trailing sentinel.
    buckets: Vec<u32>,
    bucket_shift: u32,
    len: u64,
}

impl SparseNodeIndex {
    /// Build from sealed runs given in input order (later runs win on
    /// duplicate ids).
    pub fn from_runs(runs: impl IntoIterator<Item = SealedNodeRun>) -> Self {
        let mut tagged: Vec<(usize, Segment)> = runs
            .into_iter()
            .flat_map(|r| r.segments)
            .enumerate()
            .collect();
        // Stable: ties keep input order.
        tagged.sort_by_key(|(_, s)| s.first_id);

        let mut segments: Vec<Segment> = Vec::with_capacity(tagged.len());
        let mut i = 0;
        while i < tagged.len() {
            let mut j = i + 1;
            let mut group_last = tagged[i].1.last_id;
            while j < tagged.len() && tagged[j].1.first_id <= group_last {
                group_last = group_last.max(tagged[j].1.last_id);
                j += 1;
            }
            if j == i + 1 {
                let (_, seg) = std::mem::replace(&mut tagged[i], (0, empty_segment()));
                segments.push(seg);
            } else {
                // Overlapping segments: merge, later input wins.
                let mut merged: Vec<(i64, usize, u64)> = Vec::new();
                for (order, seg) in &tagged[i..j] {
                    merged.extend(seg.entries().into_iter().map(|(id, c)| (id, *order, c)));
                }
                merged.sort_unstable_by_key(|&(id, order, _)| (id, order));
                let mut entries: Vec<(i64, u64)> = Vec::with_capacity(merged.len());
                for (id, _, c) in merged {
                    match entries.last_mut() {
                        Some(last) if last.0 == id => last.1 = c,
                        _ => entries.push((id, c)),
                    }
                }
                segments.extend(segments_from_entries(&mut entries));
            }
            i = j;
        }

        let len = segments.iter().map(|s| s.coords.len() as u64).sum();
        let first_ids = segments.iter().map(|s| s.first_id).collect();
        let (buckets, bucket_shift) = build_buckets(&segments);
        Self {
            first_ids,
            segments,
            buckets,
            bucket_shift,
            len,
        }
    }

    /// Number of nodes stored.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Approximate heap memory used, in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.segments.iter().map(Segment::heap_bytes).sum::<usize>()
            + self.segments.len() * std::mem::size_of::<Segment>()
            + self.first_ids.len() * 8
            + self.buckets.len() * 4
    }

    /// Smallest and largest node id stored.
    pub fn id_range(&self) -> Option<(i64, i64)> {
        Some((
            self.segments.first()?.first_id,
            self.segments.last()?.last_id,
        ))
    }

    #[inline]
    fn find_segment(&self, id: i64) -> Option<&Segment> {
        let n = self.segments.len();
        let (lo, hi) = if id >= 0 && !self.buckets.is_empty() {
            let b = (id as u64 >> self.bucket_shift) as usize;
            let lo = *self.buckets.get(b)? as usize;
            let hi = self
                .buckets
                .get(b + 1)
                .map_or(n, |&h| (h as usize + 1).min(n));
            (lo, hi)
        } else {
            (0, n)
        };
        let window = self.first_ids.get(lo..hi)?;
        let k = window.partition_point(|&f| f <= id);
        let seg = self.segments.get(lo + k.checked_sub(1)?)?;
        (id <= seg.last_id).then_some(seg)
    }
}

fn empty_segment() -> Segment {
    Segment {
        first_id: 0,
        last_id: 0,
        coords: Box::new([]),
        index: SegmentIndex::Contiguous,
    }
}

fn build_buckets(segments: &[Segment]) -> (Vec<u32>, u32) {
    let Some(max_id) = segments.last().map(|s| s.last_id) else {
        return (Vec::new(), MIN_BUCKET_SHIFT);
    };
    if max_id < 0 || u32::try_from(segments.len()).is_err() {
        return (Vec::new(), MIN_BUCKET_SHIFT);
    }
    let mut shift = MIN_BUCKET_SHIFT;
    while (max_id as u64 >> shift) >= MAX_BUCKETS {
        shift += 1;
    }
    let count = (max_id as u64 >> shift) as usize + 1;
    let mut buckets = Vec::with_capacity(count + 1);
    let mut seg = 0usize;
    for b in 0..count {
        let start = (b as i64) << shift;
        while seg < segments.len() && segments[seg].last_id < start {
            seg += 1;
        }
        buckets.push(seg as u32);
    }
    buckets.push(segments.len() as u32);
    (buckets, shift)
}

impl NodeLocationStore for SparseNodeIndex {
    #[inline]
    fn get(&self, node_id: i64) -> Option<FixedCoord> {
        self.find_segment(node_id)?
            .lookup(node_id)
            .and_then(FixedCoord::unpack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord_for(id: i64) -> FixedCoord {
        // Deterministic, id-dependent, always valid.
        FixedCoord::new((id % 1_800_000_000) as i32, (id % 900_000_000) as i32 / 2)
    }

    fn run(ids: impl IntoIterator<Item = i64>) -> SealedNodeRun {
        let mut r = NodeRun::default();
        for id in ids {
            r.push(id, coord_for(id));
        }
        r.seal()
    }

    #[test]
    fn contiguous_bitmap_and_offset_segments_all_resolve() {
        let contiguous = run(100..200);
        let bitmap = run((1_000..3_000).step_by(3));
        let offsets = run((0..50).map(|i| 1_000_000 + i * 100_003));
        let idx = SparseNodeIndex::from_runs([contiguous, bitmap, offsets]);
        for id in (100..200).chain((1_000..3_000).step_by(3)) {
            assert_eq!(idx.get(id), Some(coord_for(id)), "id {id}");
        }
        for i in 0..50 {
            let id = 1_000_000 + i * 100_003;
            assert_eq!(idx.get(id), Some(coord_for(id)));
        }
        for missing in [99, 200, 1_001, 2_999, 1_000_001, -5, i64::MAX] {
            assert_eq!(idx.get(missing), None, "id {missing}");
        }
        assert_eq!(idx.len(), 100 + 667 + 50);
    }

    #[test]
    fn segment_representation_choice() {
        let seg = |ids: Vec<i64>| {
            let e: Vec<_> = ids.iter().map(|&i| (i, coord_for(i).pack())).collect();
            Segment::from_sorted(&e)
        };
        assert!(matches!(
            seg((0..64).collect()).index,
            SegmentIndex::Contiguous
        ));
        assert!(matches!(
            seg((0..640).step_by(2).collect()).index,
            SegmentIndex::Bitmap { .. }
        ));
        assert!(matches!(
            seg(vec![0, 1_000_000, 2_000_000]).index,
            SegmentIndex::Offsets(_)
        ));
    }

    #[test]
    fn unsorted_runs_with_duplicates_last_write_wins() {
        let mut a = NodeRun::default();
        a.push(5, FixedCoord::new(1, 1));
        a.push(3, FixedCoord::new(2, 2));
        a.push(5, FixedCoord::new(3, 3)); // later within run
        let mut b = NodeRun::default();
        b.push(4, FixedCoord::new(4, 4));
        b.push(3, FixedCoord::new(5, 5)); // later run overrides
        let idx = SparseNodeIndex::from_runs([a.seal(), b.seal()]);
        assert_eq!(idx.get(5), Some(FixedCoord::new(3, 3)));
        assert_eq!(idx.get(3), Some(FixedCoord::new(5, 5)));
        assert_eq!(idx.get(4), Some(FixedCoord::new(4, 4)));
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn overlapping_runs_merge() {
        let idx =
            SparseNodeIndex::from_runs([run((0..1000).step_by(2)), run((1..1000).step_by(2))]);
        for id in 0..1000 {
            assert_eq!(idx.get(id), Some(coord_for(id)));
        }
    }

    #[test]
    fn negative_ids_are_supported() {
        let idx = SparseNodeIndex::from_runs([run([-10, -3, 0, 7])]);
        assert_eq!(idx.get(-10), Some(coord_for(-10)));
        assert_eq!(idx.get(-3), Some(coord_for(-3)));
        assert_eq!(idx.get(-4), None);
        assert_eq!(idx.get(7), Some(coord_for(7)));
    }

    #[test]
    fn huge_ids_and_spans() {
        let ids = [
            0,
            1,
            u32::MAX as i64 * 3,
            13_000_000_000,
            13_000_000_001,
            40_000_000_000,
        ];
        let idx = SparseNodeIndex::from_runs([run(ids)]);
        for id in ids {
            assert_eq!(idx.get(id), Some(coord_for(id)), "id {id}");
        }
        assert_eq!(idx.get(13_000_000_002), None);
    }

    #[test]
    fn empty_index() {
        let idx = SparseNodeIndex::from_runs(Vec::<SealedNodeRun>::new());
        assert!(idx.is_empty());
        assert_eq!(idx.get(0), None);
        assert_eq!(idx.id_range(), None);
    }

    #[test]
    fn many_runs_match_a_reference_map() {
        // Pseudo-random sparse ids across a wide range, split into blocks
        // like a PBF file, checked against a HashMap.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut reference = std::collections::HashMap::new();
        let mut runs = Vec::new();
        let mut id = 0i64;
        for _ in 0..200 {
            let mut r = NodeRun::default();
            for _ in 0..(next() % 500) {
                id += 1 + (next() % 5000) as i64;
                let c = coord_for(id);
                r.push(id, c);
                reference.insert(id, c);
            }
            runs.push(r.seal());
        }
        let idx = SparseNodeIndex::from_runs(runs);
        assert_eq!(idx.len(), reference.len() as u64);
        for (&id, &c) in &reference {
            assert_eq!(idx.get(id), Some(c));
            assert_eq!(idx.get(id + 1).is_some(), reference.contains_key(&(id + 1)));
        }
        // ~8 B/node plus a modest index.
        assert!(idx.heap_bytes() < reference.len() * 16 + 1_000_000);
    }
}
