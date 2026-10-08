//! Dense node location store: one 8-byte slot per possible node id.
//!
//! Slot `id` holds [`FixedCoord::pack`] of the node's location, or 0 when the
//! node is absent. Lookups are a single indexed load. Memory (or disk) cost is
//! proportional to the largest id, not the number of nodes, so this store
//! suits planet-scale inputs and persistent replication databases; for
//! regional extracts [`crate::SparseNodeIndex`] is far smaller.
//!
//! Two backings share one implementation:
//!
//! - [`DenseNodeStore::in_memory`]: anonymous memory, reserved but not
//!   committed up front (`MAP_NORESERVE`), so only pages that receive nodes
//!   use RAM.
//! - [`DenseNodeStore::create`] / [`DenseNodeStore::open`]: a file with a
//!   small header (magic, format version, capacity) followed by the slots,
//!   comparable to osm2pgsql's flat-nodes file. The file is sparse on disk
//!   until written.
//!
//! All slot access goes through `AtomicU64` with relaxed ordering, so
//! concurrent writers (parallel PBF decoding, including duplicate ids in
//! history files) can never tear a coordinate; readers see either the old or
//! the new value.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::{MmapOptions, MmapRaw};
use tracing::info;

use osmic_core::{FixedCoord, NodeLocationStore};

use crate::error::NodeStoreError;

const MAGIC: &[u8; 8] = b"OSMICND1";
const FORMAT_VERSION: u32 = 1;
/// Header length; one page, so the slot array stays page- and 8-byte-aligned.
const HEADER_LEN: usize = 4096;
const SLOT: usize = std::mem::size_of::<u64>();

/// Dense, id-indexed node location store (anonymous memory or file).
pub struct DenseNodeStore {
    /// Raw mapping: slots are only accessed through atomics derived from
    /// [`MmapRaw::as_mut_ptr`], which carries write permission.
    map: MmapRaw,
    /// Byte offset of slot 0 within `map`.
    offset: usize,
    /// Number of slots: valid ids are `0..capacity`.
    capacity: usize,
}

fn slot_bytes(max_node_id: i64) -> Result<(usize, usize), NodeStoreError> {
    let capacity = u64::try_from(max_node_id)
        .ok()
        .and_then(|m| m.checked_add(1))
        .and_then(|c| usize::try_from(c).ok())
        .ok_or(NodeStoreError::InvalidCapacity { max_node_id })?;
    let bytes = capacity
        .checked_mul(SLOT)
        .ok_or(NodeStoreError::InvalidCapacity { max_node_id })?;
    Ok((capacity, bytes))
}

impl DenseNodeStore {
    /// An anonymous-memory store for ids `0..=max_node_id`.
    ///
    /// # Errors
    ///
    /// [`NodeStoreError::InvalidCapacity`] for a negative or oversized
    /// `max_node_id`; [`NodeStoreError::Io`] if the mapping fails.
    pub fn in_memory(max_node_id: i64) -> Result<Self, NodeStoreError> {
        let (capacity, bytes) = slot_bytes(max_node_id)?;
        info!(
            max_node_id,
            gib = bytes as f64 / f64::from(1u32 << 30),
            "Reserving dense in-memory node store"
        );
        let map = MmapOptions::new()
            .len(bytes.max(SLOT))
            .no_reserve_swap()
            .map_anon()?;
        Ok(Self {
            map: MmapRaw::from(map),
            offset: 0,
            capacity,
        })
    }

    /// Create (or truncate) a file-backed store for ids `0..=max_node_id`.
    ///
    /// # Errors
    ///
    /// [`NodeStoreError::InvalidCapacity`] for a negative or oversized
    /// `max_node_id`; [`NodeStoreError::Io`] if the file cannot be created,
    /// sized or mapped.
    pub fn create(path: &Path, max_node_id: i64) -> Result<Self, NodeStoreError> {
        let (capacity, bytes) = slot_bytes(max_node_id)?;
        let total = bytes
            .checked_add(HEADER_LEN)
            .ok_or(NodeStoreError::InvalidCapacity { max_node_id })?;
        info!(
            path = %path.display(),
            max_node_id,
            gib = total as f64 / f64::from(1u32 << 30),
            "Creating file-backed dense node store"
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(total as u64)?;
        // SAFETY: the file was just created/truncated by us and stays open
        // for the lifetime of the mapping; osmic never truncates it while
        // mapped. Concurrent modification by other processes is outside the
        // store's contract (documented on `open`).
        let mut map = unsafe { MmapOptions::new().len(total).map_mut(&file)? };
        map[..8].copy_from_slice(MAGIC);
        map[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        map[16..24].copy_from_slice(&(capacity as u64).to_le_bytes());
        Ok(Self {
            map: MmapRaw::from(map),
            offset: HEADER_LEN,
            capacity,
        })
    }

    /// Open an existing file-backed store.
    ///
    /// The file must not be truncated or rewritten by another process while
    /// it is open: it is memory-mapped.
    ///
    /// # Errors
    ///
    /// [`NodeStoreError::Io`] if the file cannot be opened read-write or
    /// mapped; [`NodeStoreError::InvalidFile`] if it is not a store written
    /// by [`DenseNodeStore::create`] in this format version.
    pub fn open(path: &Path) -> Result<Self, NodeStoreError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| NodeStoreError::InvalidFile("file larger than address space".into()))?;
        if len < HEADER_LEN {
            return Err(NodeStoreError::InvalidFile(format!(
                "{} is {len} bytes, smaller than the {HEADER_LEN}-byte header",
                path.display()
            )));
        }
        // SAFETY: see `create`; same ownership contract.
        let map = unsafe { MmapOptions::new().len(len).map_mut(&file)? };
        if &map[..8] != MAGIC {
            return Err(NodeStoreError::InvalidFile(format!(
                "{} is not an osmic node store (bad magic)",
                path.display()
            )));
        }
        let version = u32::from_le_bytes(map[8..12].try_into().unwrap_or_default());
        if version != FORMAT_VERSION {
            return Err(NodeStoreError::InvalidFile(format!(
                "{}: unsupported node store version {version} (expected {FORMAT_VERSION})",
                path.display()
            )));
        }
        let capacity = u64::from_le_bytes(map[16..24].try_into().unwrap_or_default());
        let capacity = usize::try_from(capacity)
            .map_err(|_| NodeStoreError::InvalidFile("capacity overflows usize".into()))?;
        let needed = capacity
            .checked_mul(SLOT)
            .and_then(|b| b.checked_add(HEADER_LEN))
            .ok_or_else(|| NodeStoreError::InvalidFile("capacity overflows usize".into()))?;
        if needed != len {
            return Err(NodeStoreError::InvalidFile(format!(
                "{}: header says {capacity} slots ({needed} bytes) but file is {len} bytes",
                path.display()
            )));
        }
        Ok(Self {
            map: MmapRaw::from(map),
            offset: HEADER_LEN,
            capacity,
        })
    }

    /// Number of slots; valid node ids are `0..capacity()`.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn slots(&self) -> &[AtomicU64] {
        let base = self.map.as_mut_ptr().wrapping_add(self.offset);
        // SAFETY: `base` comes from `MmapRaw::as_mut_ptr`, so it may be
        // written through (no shared-reference provenance), and lies inside
        // the live mapping (offset 0 or one page), page-aligned and hence
        // 8-byte aligned. The mapping holds exactly
        // `capacity` 8-byte slots after `offset` (checked at construction)
        // and lives as long as `self`. All access to the slot bytes goes
        // through these atomics — the raw bytes are never exposed — so
        // shared mutation is sound.
        unsafe { std::slice::from_raw_parts(base.cast::<AtomicU64>(), self.capacity) }
    }

    fn slot(&self, node_id: i64) -> Result<&AtomicU64, NodeStoreError> {
        usize::try_from(node_id)
            .ok()
            .and_then(|i| self.slots().get(i))
            .ok_or(NodeStoreError::IdOutOfRange {
                id: node_id,
                capacity: self.capacity,
            })
    }

    /// Store a node location. Errors if `node_id` is negative or beyond the
    /// capacity — never silently drops the node.
    ///
    /// # Errors
    ///
    /// [`NodeStoreError::IdOutOfRange`] if `node_id` is not in
    /// `0..capacity()`.
    pub fn set(&self, node_id: i64, coord: FixedCoord) -> Result<(), NodeStoreError> {
        self.slot(node_id)?.store(coord.pack(), Ordering::Relaxed);
        Ok(())
    }

    /// Remove a node (e.g. deleted by a replication diff).
    ///
    /// # Errors
    ///
    /// [`NodeStoreError::IdOutOfRange`] if `node_id` is not in
    /// `0..capacity()`.
    pub fn remove(&self, node_id: i64) -> Result<(), NodeStoreError> {
        self.slot(node_id)?.store(0, Ordering::Relaxed);
        Ok(())
    }

    /// Flush a file-backed store to disk (no-op for in-memory stores).
    ///
    /// # Errors
    ///
    /// [`NodeStoreError::Io`] if the flush fails.
    pub fn flush(&self) -> Result<(), NodeStoreError> {
        if self.offset > 0 {
            self.map.flush()?;
        }
        Ok(())
    }
}

impl NodeLocationStore for DenseNodeStore {
    fn get(&self, node_id: i64) -> Option<FixedCoord> {
        let slot = self.slot(node_id).ok()?;
        FixedCoord::unpack(slot.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(lon: i32, lat: i32) -> FixedCoord {
        FixedCoord::new(lon, lat)
    }

    #[test]
    fn in_memory_round_trip_is_exact() {
        let store = DenseNodeStore::in_memory(1_000).expect("create");
        let c = fc(134_049_540, 525_200_070);
        store.set(42, c).expect("in range");
        assert_eq!(store.get(42), Some(c));
        assert_eq!(store.get(7), None);
    }

    #[test]
    fn origin_is_a_real_location() {
        let store = DenseNodeStore::in_memory(10).expect("create");
        store.set(5, fc(0, 0)).expect("in range");
        assert_eq!(store.get(5), Some(fc(0, 0)));
    }

    #[test]
    fn out_of_range_ids_are_errors_not_silent_drops() {
        let store = DenseNodeStore::in_memory(9).expect("create");
        assert!(store.set(9, fc(1, 1)).is_ok());
        assert!(matches!(
            store.set(10, fc(1, 1)),
            Err(NodeStoreError::IdOutOfRange {
                id: 10,
                capacity: 10
            })
        ));
        assert!(store.set(-1, fc(1, 1)).is_err());
        assert_eq!(store.get(10), None);
        assert_eq!(store.get(-1), None);
    }

    #[test]
    fn invalid_capacity_is_rejected() {
        assert!(DenseNodeStore::in_memory(-1).is_err());
        assert!(DenseNodeStore::in_memory(i64::MAX).is_err());
    }

    #[test]
    fn remove_clears_slot() {
        let store = DenseNodeStore::in_memory(10).expect("create");
        store.set(3, fc(10, 20)).expect("in range");
        store.remove(3).expect("in range");
        assert_eq!(store.get(3), None);
    }

    #[test]
    fn file_store_persists_and_validates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nodes.bin");
        {
            let store = DenseNodeStore::create(&path, 100).expect("create");
            store
                .set(99, fc(-1_234_567_890, 456_789_012))
                .expect("in range");
            store.flush().expect("flush");
        }
        let store = DenseNodeStore::open(&path).expect("open");
        assert_eq!(store.capacity(), 101);
        assert_eq!(store.get(99), Some(fc(-1_234_567_890, 456_789_012)));

        let bogus = dir.path().join("bogus.bin");
        std::fs::write(&bogus, vec![0u8; HEADER_LEN + 8]).expect("write");
        assert!(matches!(
            DenseNodeStore::open(&bogus),
            Err(NodeStoreError::InvalidFile(_))
        ));
        let tiny = dir.path().join("tiny.bin");
        std::fs::write(&tiny, b"x").expect("write");
        assert!(DenseNodeStore::open(&tiny).is_err());
    }

    #[test]
    fn concurrent_writers_never_tear() {
        use std::sync::Arc;
        let store = Arc::new(DenseNodeStore::in_memory(64).expect("create"));
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for i in 0..10_000 {
                        // Every thread hammers the same slots with
                        // self-consistent (lon == lat) values.
                        let v = t * 100_000 + i;
                        store.set(i64::from(i % 64), fc(v, v)).expect("in range");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        for id in 0..64 {
            let c = store.get(id).expect("written");
            assert_eq!(c.lon, c.lat, "torn write at {id}");
        }
    }
}
