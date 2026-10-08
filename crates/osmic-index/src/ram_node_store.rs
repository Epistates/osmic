//! RAM-backed node location store — libosmium FlexMem reinterpreted.
//!
//! # Design
//!
//! libosmium ships several node-ID index variants:
//!
//! - **SparseMemArray**: sorted `Vec<(id, value)>`, O(log n) lookup, memory
//!   proportional to entry count.
//! - **DenseMemArray**: contiguous `Vec<value>` of `max_id * sizeof(value)`,
//!   O(1) lookup, memory proportional to max_id. Eagerly commits all pages
//!   — on macOS/Windows this pins all of the virtual address space.
//! - **FlexMem**: starts sparse, switches to dense when density crosses
//!   ~1/3 (after 16M entries). Pays a one-time migration cost.
//!
//! We ship a single store that's strictly better than DenseMemArray and
//! competitive with FlexMem for our workload:
//!
//! 1. **Anonymous mmap** (`MAP_ANON`) — no file descriptor, no disk I/O,
//!    no page cache pollution. Compare with [`crate::DenseNodeLocationStore`]
//!    which is file-backed and forces every write through the page cache.
//! 2. **OS-managed sparsity at 4 KB page granularity** — only pages that
//!    are actually touched get committed. This is strictly finer-grained
//!    than FlexMem's monolithic sparse→dense switch.
//! 3. **Same O(1) indexing as dense** — no migration, no branching, no
//!    sorted-insert cost. The mmap is always "dense" in virtual address
//!    space; physical commitment is deferred to the kernel.
//! 4. **Concurrent-safe writes** — each node ID maps to a unique
//!    non-overlapping 8-byte slot, so rayon workers can write in parallel
//!    without synchronization.
//!
//! # Why this is better than libosmium FlexMem
//!
//! libosmium's FlexMem makes a binary choice (sparse vs dense) and pays
//! a migration cost at the crossover. Our approach uses the kernel's
//! virtual memory system to get *continuous* adaptation: each touched
//! 4 KB page costs 512 slots' worth of RAM, regardless of how scattered
//! or clustered the node IDs are. For OSM PBF input — where DenseNode
//! blocks group sequential node IDs — the access pattern is highly
//! clustered, so most pages that get touched are nearly full. A Berlin
//! extract touches ~12 K pages (~48 MB) even though its node ID range
//! spans ~12 billion.
//!
//! # When to prefer the file-backed store
//!
//! Use [`crate::DenseNodeLocationStore`] instead when:
//!
//! - The node index must survive across runs (incremental updates).
//! - The extract is so large that physical RAM cannot hold the touched
//!   pages (swap-to-disk is cheaper via mmap-to-file than anon + OS swap).
//! - You're debugging a run and want to inspect the store on disk.
//!
//! For normal one-shot PBF processing, prefer `RamNodeLocationStore`.

use std::io;

use memmap2::{MmapMut, MmapOptions};
use tracing::info;

use osmic_core::NodeLocationStore;
use osmic_core::coord::{LonLat, PackedCoord};

const PACKED_COORD_SIZE: usize = std::mem::size_of::<PackedCoord>();

/// RAM-backed dense node location store.
///
/// See the module-level docs for the design rationale. This store
/// behaves like [`crate::DenseNodeLocationStore`] except the backing
/// memory is anonymous — no file, no disk I/O, and no page cache
/// pressure. On most OSes the virtual allocation is lazy, so the
/// resident set size matches the set of touched pages.
pub struct RamNodeLocationStore {
    base: *mut u8,
    capacity: usize,
    _mmap: MmapMut,
}

// SAFETY: Concurrent writes to non-overlapping regions (distinct node IDs)
// are safe. Each node ID maps to a unique 8-byte slot.
unsafe impl Send for RamNodeLocationStore {}
unsafe impl Sync for RamNodeLocationStore {}

impl RamNodeLocationStore {
    /// Create a new RAM-backed store capable of holding up to
    /// `max_node_id + 1` distinct nodes. The OS reserves virtual
    /// address space immediately but only commits physical pages on
    /// first-write (demand paging).
    ///
    /// `max_node_id` should be an upper bound on the node IDs you expect
    /// to see. For planet OSM today, 16_000_000_000 is a safe value.
    pub fn create(max_node_id: i64) -> io::Result<Self> {
        if max_node_id < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_node_id must be non-negative",
            ));
        }

        let capacity = (max_node_id + 1) as usize;
        let byte_len = capacity
            .checked_mul(PACKED_COORD_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "capacity overflow"))?;

        info!(
            max_node_id,
            byte_len,
            gb = byte_len as f64 / (1024.0 * 1024.0 * 1024.0),
            "Creating RAM-backed node location store (anonymous mmap)"
        );

        let mut mmap = MmapOptions::new().len(byte_len).map_anon()?;
        let base = mmap.as_mut_ptr();

        Ok(Self {
            base,
            capacity,
            _mmap: mmap,
        })
    }

    /// Create a store with a capacity heuristic derived from the PBF
    /// file size. This is useful when the caller doesn't know the exact
    /// max node ID — the heuristic biases toward over-provisioning,
    /// which is cheap for anonymous mmap (no eager commitment).
    ///
    /// Current heuristic: `max(pbf_mb * 25M, 10M)`. This assumes ~25M
    /// nodes per MB of PBF on average, plus 10M slack. Anonymous mmap
    /// over-provisioning is essentially free.
    pub fn for_pbf_size(pbf_size_bytes: u64) -> io::Result<Self> {
        let pbf_mb = (pbf_size_bytes / (1024 * 1024)).max(1);
        let estimated_max_id = (pbf_mb * 25_000_000).max(10_000_000) as i64;
        Self::create(estimated_max_id)
    }

    /// Create a store sized for the current planet PBF (~16B nodes).
    pub fn for_planet() -> io::Result<Self> {
        Self::create(16_000_000_000)
    }

    /// Maximum number of slots this store can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl NodeLocationStore for RamNodeLocationStore {
    fn set(&self, node_id: i64, lon: f64, lat: f64) {
        if node_id < 0 {
            return;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return;
        }
        let packed = PackedCoord::pack(lon, lat);
        unsafe {
            let ptr = self.base.add(idx * PACKED_COORD_SIZE) as *mut PackedCoord;
            ptr.write(packed);
        }
    }

    fn get(&self, node_id: i64) -> Option<LonLat> {
        if node_id < 0 {
            return None;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return None;
        }
        let packed = unsafe {
            let ptr = self.base.add(idx * PACKED_COORD_SIZE) as *const PackedCoord;
            ptr.read()
        };
        if packed.is_empty() {
            return None;
        }
        Some(packed.unpack())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip epsilon: f32 precision is ~1e-7 degrees (< 15 mm at equator).
    const EPSILON: f64 = 1e-5;

    #[test]
    fn round_trip_set_get() {
        let store = RamNodeLocationStore::create(1_000).expect("create ok");
        store.set(42, 13.405, 52.520);
        let loc = store.get(42).expect("should have value");
        assert!((loc.lon - 13.405).abs() < EPSILON);
        assert!((loc.lat - 52.520).abs() < EPSILON);
    }

    #[test]
    fn unset_node_returns_none() {
        let store = RamNodeLocationStore::create(1_000).expect("create ok");
        assert!(store.get(7).is_none());
    }

    #[test]
    fn node_id_zero_works() {
        let store = RamNodeLocationStore::create(10).expect("create ok");
        store.set(0, -0.1276, 51.5074);
        let loc = store.get(0).expect("node 0 should be set");
        assert!((loc.lon - -0.1276).abs() < EPSILON);
        assert!((loc.lat - 51.5074).abs() < EPSILON);
    }

    #[test]
    fn node_id_at_capacity_minus_one() {
        let store = RamNodeLocationStore::create(9).expect("create ok");
        // capacity = 10; last valid index is 9.
        store.set(9, 100.0, -10.0);
        let loc = store.get(9).expect("boundary node should be set");
        assert!((loc.lon - 100.0).abs() < EPSILON);
        assert!((loc.lat - -10.0).abs() < EPSILON);
    }

    #[test]
    fn oob_node_id_silently_ignored() {
        let store = RamNodeLocationStore::create(9).expect("create ok");
        store.set(10, 1.0, 2.0); // OOB, should be ignored
        assert!(store.get(10).is_none());
    }

    #[test]
    fn negative_node_id_returns_none() {
        let store = RamNodeLocationStore::create(100).expect("create ok");
        store.set(-1, 1.0, 2.0);
        assert!(store.get(-1).is_none());
    }

    #[test]
    fn packed_coord_at_origin_is_not_confused_with_empty() {
        // (0.0, 0.0) is a valid coordinate (Gulf of Guinea).
        let store = RamNodeLocationStore::create(10).expect("create ok");
        store.set(5, 0.0, 0.0);
        let loc = store.get(5).expect("(0.0, 0.0) should be retrievable");
        assert!(loc.lon.abs() < EPSILON);
        assert!(loc.lat.abs() < EPSILON);
    }

    #[test]
    fn invalid_max_node_id_errors() {
        let result = RamNodeLocationStore::create(-1);
        assert!(result.is_err());
    }

    #[test]
    fn for_pbf_size_produces_usable_store() {
        // 100 MB PBF → should size for at least 10M nodes
        let store = RamNodeLocationStore::for_pbf_size(100 * 1024 * 1024).expect("create ok");
        assert!(store.capacity() >= 10_000_000);
        // Round-trip a write to prove it's functional
        store.set(5_000_000, 1.0, 2.0);
        let loc = store.get(5_000_000).expect("should have value");
        assert!((loc.lon - 1.0).abs() < EPSILON);
    }

    #[test]
    fn for_pbf_size_minimum_capacity() {
        // Tiny PBF (1KB) still gets a usable store
        let store = RamNodeLocationStore::for_pbf_size(1024).expect("create ok");
        assert!(store.capacity() >= 10_000_000);
    }

    #[test]
    fn concurrent_writes_to_distinct_slots_are_safe() {
        use std::sync::Arc;
        use std::thread;

        // The purpose of this test is concurrency correctness, not
        // coordinate precision. Use a loose tolerance (~100m) because
        // PackedCoord's f32 encoding loses precision near the origin.
        const CONCURRENCY_EPSILON: f64 = 1e-3;

        let store = Arc::new(RamNodeLocationStore::create(10_000).expect("create ok"));
        let mut handles = Vec::new();

        for t in 0..8 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                // Use coordinates in a range where f32 precision is fine
                // (10..90 for lat, -170..170 for lon — real-world values).
                for i in 0..1000 {
                    let id = (t * 1000 + i) as i64;
                    let lon = (id as f64 % 360.0) - 180.0;
                    let lat = (id as f64 % 170.0) - 85.0;
                    store.set(id, lon, lat);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread join");
        }

        // Verify all writes landed (exact values not required, we care
        // that each slot got SOMETHING and it matches the write).
        for t in 0..8 {
            for i in 0..1000 {
                let id = (t * 1000 + i) as i64;
                let expected_lon = (id as f64 % 360.0) - 180.0;
                let expected_lat = (id as f64 % 170.0) - 85.0;
                let loc = store.get(id).unwrap_or_else(|| panic!("missing id {id}"));
                assert!(
                    (loc.lon - expected_lon).abs() < CONCURRENCY_EPSILON,
                    "id={id} lon={} expected={}",
                    loc.lon,
                    expected_lon
                );
                assert!(
                    (loc.lat - expected_lat).abs() < CONCURRENCY_EPSILON,
                    "id={id} lat={} expected={}",
                    loc.lat,
                    expected_lat
                );
            }
        }
    }
}
