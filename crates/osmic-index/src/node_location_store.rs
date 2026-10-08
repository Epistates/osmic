use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use memmap2::{MmapMut, MmapOptions};
use tracing::info;

use osmic_core::NodeLocationStore;
use osmic_core::coord::{LonLat, PackedCoord};

const PACKED_COORD_SIZE: usize = std::mem::size_of::<PackedCoord>(); // 8 bytes

/// Memory-mapped dense array for storing node locations.
///
/// Node IDs index directly into the array: `offset = node_id * 8`.
/// For 870M nodes at 8 bytes each ≈ 7 GB resident memory.
/// The OS pages in data on demand via the mmap; unwritten regions
/// stay sparse on disk (APFS/ext4/btrfs).
///
/// # Thread Safety
///
/// Concurrent writes are safe because each node ID maps to a unique
/// non-overlapping 8-byte region, and node IDs are unique in PBF files.
pub struct DenseNodeLocationStore {
    base: *mut u8,
    capacity: usize,
    _mmap: MmapMut,
    _file: File,
}

// SAFETY: Concurrent writes to non-overlapping regions (distinct node IDs)
// are safe. Each node ID maps to a unique 8-byte slot.
unsafe impl Send for DenseNodeLocationStore {}
unsafe impl Sync for DenseNodeLocationStore {}

impl DenseNodeLocationStore {
    /// Create a new store backed by a memory-mapped file.
    ///
    /// `max_node_id` determines the file size: `(max_node_id + 1) * 8` bytes.
    /// For a 2025 North America extract, use ~13_000_000_000.
    pub fn create(path: &Path, max_node_id: i64) -> io::Result<Self> {
        let capacity = (max_node_id + 1) as usize;
        let byte_len = capacity * PACKED_COORD_SIZE;

        info!(
            max_node_id,
            byte_len,
            gb = byte_len as f64 / (1024.0 * 1024.0 * 1024.0),
            "Creating dense node location store"
        );

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Extend file to full size (sparse on supported filesystems)
        file.set_len(byte_len as u64)?;

        let mut mmap = unsafe { MmapOptions::new().len(byte_len).map_mut(&file)? };
        let base = mmap.as_mut_ptr();

        Ok(Self {
            base,
            capacity,
            _mmap: mmap,
            _file: file,
        })
    }

    /// Open an existing store from a memory-mapped file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let metadata = file.metadata()?;
        let byte_len = metadata.len() as usize;
        let capacity = byte_len / PACKED_COORD_SIZE;

        let mut mmap = unsafe { MmapOptions::new().len(byte_len).map_mut(&file)? };
        let base = mmap.as_mut_ptr();

        Ok(Self {
            base,
            capacity,
            _mmap: mmap,
            _file: file,
        })
    }

    /// Maximum number of nodes this store can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Flush written data to disk.
    pub fn flush(&self) -> io::Result<()> {
        self._mmap.flush()
    }
}

impl NodeLocationStore for DenseNodeLocationStore {
    /// Store coordinates for a node. Thread-safe for distinct node IDs.
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

    /// Retrieve coordinates for a node.
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
    use osmic_core::NodeLocationStore;

    /// Round-trip epsilon: f32 precision is ~1e-7 degrees (< 15 mm at equator).
    const EPSILON: f64 = 1e-5;

    fn store_with_capacity(cap: i64) -> (DenseNodeLocationStore, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = DenseNodeLocationStore::create(tmp.path(), cap).expect("create store");
        (store, tmp)
    }

    #[test]
    fn round_trip_set_get() {
        let (store, _tmp) = store_with_capacity(100);
        store.set(42, 13.405, 52.520);
        let loc = store.get(42).expect("should have value");
        assert!((loc.lon - 13.405).abs() < EPSILON);
        assert!((loc.lat - 52.520).abs() < EPSILON);
    }

    #[test]
    fn unset_node_returns_none() {
        let (store, _tmp) = store_with_capacity(100);
        // Node 7 was never written; the zero-filled mmap page is detected as empty.
        assert!(store.get(7).is_none());
    }

    #[test]
    fn node_id_zero_works() {
        let (store, _tmp) = store_with_capacity(10);
        store.set(0, -0.1276, 51.5074);
        let loc = store.get(0).expect("node 0 should be set");
        assert!((loc.lon - -0.1276).abs() < EPSILON);
        assert!((loc.lat - 51.5074).abs() < EPSILON);
    }

    #[test]
    fn node_id_at_capacity_minus_one() {
        let max_node_id: i64 = 9;
        let (store, _tmp) = store_with_capacity(max_node_id);
        // capacity = max_node_id + 1 = 10; last valid index is 9.
        store.set(max_node_id, 100.0, -10.0);
        let loc = store.get(max_node_id).expect("boundary node should be set");
        assert!((loc.lon - 100.0).abs() < EPSILON);
        assert!((loc.lat - -10.0).abs() < EPSILON);
    }

    #[test]
    fn node_id_at_capacity_returns_none_on_get() {
        let (store, _tmp) = store_with_capacity(9); // capacity = 10, valid ids 0..=9
        // node_id 10 == capacity, which is out of bounds
        store.set(10, 1.0, 2.0); // silently ignored
        assert!(store.get(10).is_none());
    }

    #[test]
    fn negative_node_id_returns_none() {
        let (store, _tmp) = store_with_capacity(100);
        store.set(-1, 1.0, 2.0); // should be silently ignored
        assert!(store.get(-1).is_none());
    }

    #[test]
    fn packed_coord_at_origin_is_not_confused_with_empty() {
        // (0.0, 0.0) is a valid coordinate (Gulf of Guinea).
        // The offset encoding must not treat it as the unwritten sentinel.
        let (store, _tmp) = store_with_capacity(10);
        store.set(5, 0.0, 0.0);
        let loc = store.get(5).expect("(0.0, 0.0) should be retrievable");
        assert!(loc.lon.abs() < EPSILON);
        assert!(loc.lat.abs() < EPSILON);
    }
}
