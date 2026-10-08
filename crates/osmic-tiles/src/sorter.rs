//! Parallel external merge sort for variable-length records.
//!
//! Many threads append records through [`SortWriter`]s, each filling its own
//! in-memory chunk. A full chunk is sorted by the thread that filled it and
//! spilled to a file in a private temporary directory. [`ExternalSorter::finish`]
//! merges the spilled files and any chunks still in memory with a k-way
//! heap merge, so only the memory budget plus one buffered record per
//! source is resident. Small inputs never touch the disk.
//!
//! Ordering is total — `(key, secondary, payload bytes)` — so output is
//! identical regardless of how work was split across threads. The
//! temporary directory is removed when the sorter or its output iterator is
//! dropped, including on error.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use tempfile::TempDir;

/// Maximum files merged at once; larger sets are merged in rounds to stay
/// under the default open-file limit (256 on macOS).
const MAX_FAN_IN: usize = 128;
const IO_BUFFER: usize = 1 << 20;
const HEADER: usize = 20;

/// A sorted record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: u64,
    pub secondary: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    key: u64,
    secondary: u64,
    offset: usize,
    len: u32,
}

#[derive(Debug, Default)]
struct Chunk {
    entries: Vec<Entry>,
    data: Vec<u8>,
}

impl Chunk {
    fn bytes(&self) -> usize {
        self.data.len() + self.entries.len() * std::mem::size_of::<Entry>()
    }

    fn payload(&self, e: &Entry) -> &[u8] {
        &self.data[e.offset..e.offset + e.len as usize]
    }

    fn sort(&mut self) {
        let data = &self.data;
        self.entries.sort_unstable_by(|a, b| {
            (a.key, a.secondary)
                .cmp(&(b.key, b.secondary))
                .then_with(|| {
                    data[a.offset..a.offset + a.len as usize]
                        .cmp(&data[b.offset..b.offset + b.len as usize])
                })
        });
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.data.clear();
    }
}

/// Parallel external sorter. See the module docs.
pub struct ExternalSorter {
    dir: TempDir,
    chunk_bytes: usize,
    fan_in: usize,
    pool: Mutex<Vec<Chunk>>,
    files: Mutex<Vec<PathBuf>>,
    next_file: AtomicUsize,
    records: AtomicU64,
    spilled_bytes: AtomicU64,
}

impl ExternalSorter {
    /// Create a sorter whose temporary files live in a fresh directory
    /// under `temp_parent` (the system temp dir if `None`).
    ///
    /// `memory_budget` bounds the bytes buffered in memory across all
    /// writers; each writer spills once its chunk reaches
    /// `memory_budget / (2 × threads)` (clamped to 16 MiB–512 MiB).
    pub fn new(temp_parent: Option<&Path>, memory_budget: usize) -> io::Result<Self> {
        let prefix = format!("{}sort-", osmic_core::fs::temp_file_prefix());
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix);
        let dir = match temp_parent {
            Some(p) => builder.tempdir_in(p)?,
            None => builder.tempdir()?,
        };
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk_bytes = (memory_budget / (2 * threads)).clamp(16 << 20, 512 << 20);
        Ok(Self {
            dir,
            chunk_bytes,
            fan_in: MAX_FAN_IN,
            pool: Mutex::new(Vec::new()),
            files: Mutex::new(Vec::new()),
            next_file: AtomicUsize::new(0),
            records: AtomicU64::new(0),
            spilled_bytes: AtomicU64::new(0),
        })
    }

    /// Directory holding temporary files (removed on drop).
    pub fn temp_dir(&self) -> &Path {
        self.dir.path()
    }

    /// A writer for the calling thread. Writers are cheap; take one per
    /// batch of work.
    pub fn writer(&self) -> SortWriter<'_> {
        let chunk = self
            .pool
            .lock()
            .ok()
            .and_then(|mut p| p.pop())
            .unwrap_or_default();
        SortWriter {
            sorter: self,
            chunk: Some(chunk),
        }
    }

    /// Records added so far.
    pub fn records(&self) -> u64 {
        self.records.load(AtomicOrdering::Relaxed)
    }

    /// Bytes written to temporary files so far.
    pub fn spilled_bytes(&self) -> u64 {
        self.spilled_bytes.load(AtomicOrdering::Relaxed)
    }

    fn new_file(&self) -> PathBuf {
        let n = self.next_file.fetch_add(1, AtomicOrdering::Relaxed);
        self.dir.path().join(format!("chunk-{n:06}.bin"))
    }

    fn spill(&self, chunk: &mut Chunk) -> io::Result<()> {
        chunk.sort();
        let path = self.new_file();
        let mut w = BufWriter::with_capacity(IO_BUFFER, File::create(&path)?);
        let mut written = 0u64;
        for e in &chunk.entries {
            write_record(&mut w, e.key, e.secondary, chunk.payload(e))?;
            written += HEADER as u64 + u64::from(e.len);
        }
        w.flush()?;
        chunk.clear();
        self.spilled_bytes
            .fetch_add(written, AtomicOrdering::Relaxed);
        self.files
            .lock()
            .map_err(|_| io::Error::other("sorter file list poisoned"))?
            .push(path);
        Ok(())
    }

    /// Merge everything added so far into a sorted stream. All writers must
    /// have been dropped.
    pub fn finish(self) -> io::Result<SortedRecords> {
        let mut chunks = self
            .pool
            .into_inner()
            .map_err(|_| io::Error::other("sorter pool poisoned"))?;
        let mut files = self
            .files
            .into_inner()
            .map_err(|_| io::Error::other("sorter file list poisoned"))?;
        files.sort();
        chunks.retain(|c| !c.entries.is_empty());
        for c in &mut chunks {
            c.sort();
        }

        // Merge files in rounds until the final merge's fan-in fits.
        let mut round = 0usize;
        while files.len() + chunks.len() > self.fan_in {
            let group: Vec<PathBuf> = files.drain(..self.fan_in.min(files.len())).collect();
            let out = self.dir.path().join(format!("merge-{round:04}.bin"));
            round += 1;
            let mut w = BufWriter::with_capacity(IO_BUFFER, File::create(&out)?);
            let mut merged = SortedRecords::open(group.clone(), Vec::new(), None)?;
            for r in &mut merged {
                let r = r?;
                write_record(&mut w, r.key, r.secondary, &r.payload)?;
            }
            w.flush()?;
            drop(merged);
            for p in group {
                std::fs::remove_file(p)?;
            }
            files.push(out);
        }
        SortedRecords::open(files, chunks, Some(self.dir))
    }
}

/// A per-thread handle for adding records. Returns its buffer to the
/// sorter's pool when dropped.
pub struct SortWriter<'a> {
    sorter: &'a ExternalSorter,
    chunk: Option<Chunk>,
}

impl SortWriter<'_> {
    /// Add a record whose payload is written by `encode` directly into the
    /// sort buffer.
    pub fn push_with(
        &mut self,
        key: u64,
        secondary: u64,
        encode: impl FnOnce(&mut Vec<u8>),
    ) -> io::Result<()> {
        let chunk = self.chunk.get_or_insert_with(Chunk::default);
        let offset = chunk.data.len();
        encode(&mut chunk.data);
        let len = u32::try_from(chunk.data.len() - offset)
            .map_err(|_| io::Error::other("sort record larger than 4 GiB"))?;
        chunk.entries.push(Entry {
            key,
            secondary,
            offset,
            len,
        });
        self.sorter.records.fetch_add(1, AtomicOrdering::Relaxed);
        if chunk.bytes() >= self.sorter.chunk_bytes {
            self.sorter.spill(chunk)?;
        }
        Ok(())
    }

    /// Add a record.
    pub fn push(&mut self, key: u64, secondary: u64, payload: &[u8]) -> io::Result<()> {
        self.push_with(key, secondary, |buf| buf.extend_from_slice(payload))
    }
}

impl Drop for SortWriter<'_> {
    fn drop(&mut self) {
        if let Some(chunk) = self.chunk.take()
            && let Ok(mut pool) = self.sorter.pool.lock()
        {
            pool.push(chunk);
        }
    }
}

fn write_record(w: &mut impl Write, key: u64, secondary: u64, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("record too large"))?;
    w.write_all(&key.to_le_bytes())?;
    w.write_all(&secondary.to_le_bytes())?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)
}

/// Read one record; `Ok(None)` only at a clean end of file. A file that
/// ends mid-record is an error (e.g. a disk filled up while spilling).
fn read_record(r: &mut impl Read) -> io::Result<Option<Record>> {
    let mut header = [0u8; HEADER];
    let n = r.read(&mut header[..1])?;
    if n == 0 {
        return Ok(None);
    }
    r.read_exact(&mut header[1..])?;
    let key = u64::from_le_bytes(header[0..8].try_into().map_err(io::Error::other)?);
    let secondary = u64::from_le_bytes(header[8..16].try_into().map_err(io::Error::other)?);
    let len = u32::from_le_bytes(header[16..20].try_into().map_err(io::Error::other)?) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Record {
        key,
        secondary,
        payload,
    }))
}

enum Source {
    Memory { chunk: Chunk, pos: usize },
    File(BufReader<File>),
}

impl Source {
    fn next(&mut self) -> io::Result<Option<Record>> {
        match self {
            Self::Memory { chunk, pos } => {
                let Some(e) = chunk.entries.get(*pos).copied() else {
                    return Ok(None);
                };
                *pos += 1;
                Ok(Some(Record {
                    key: e.key,
                    secondary: e.secondary,
                    payload: chunk.payload(&e).to_vec(),
                }))
            }
            Self::File(r) => read_record(r),
        }
    }
}

struct HeapItem {
    record: Record,
    source: usize,
}

impl HeapItem {
    fn order(&self, other: &Self) -> Ordering {
        (
            self.record.key,
            self.record.secondary,
            &self.record.payload,
            self.source,
        )
            .cmp(&(
                other.record.key,
                other.record.secondary,
                &other.record.payload,
                other.source,
            ))
    }
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.order(other) == Ordering::Equal
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; reverse for ascending output.
        other.order(self)
    }
}

/// Records in sorted order. Yields `Err` (and then stops) on I/O errors.
pub struct SortedRecords {
    heap: BinaryHeap<HeapItem>,
    sources: Vec<Source>,
    failed: bool,
    _dir: Option<TempDir>,
}

impl SortedRecords {
    fn open(files: Vec<PathBuf>, chunks: Vec<Chunk>, dir: Option<TempDir>) -> io::Result<Self> {
        let mut sources: Vec<Source> = Vec::with_capacity(files.len() + chunks.len());
        for p in files {
            sources.push(Source::File(BufReader::with_capacity(
                IO_BUFFER,
                File::open(p)?,
            )));
        }
        sources.extend(
            chunks
                .into_iter()
                .map(|chunk| Source::Memory { chunk, pos: 0 }),
        );
        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (i, s) in sources.iter_mut().enumerate() {
            if let Some(record) = s.next()? {
                heap.push(HeapItem { record, source: i });
            }
        }
        Ok(Self {
            heap,
            sources,
            failed: false,
            _dir: dir,
        })
    }
}

impl Iterator for SortedRecords {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let top = self.heap.pop()?;
        match self.sources[top.source].next() {
            Ok(Some(record)) => self.heap.push(HeapItem {
                record,
                source: top.source,
            }),
            Ok(None) => {}
            Err(e) => {
                self.failed = true;
                return Some(Err(e));
            }
        }
        Some(Ok(top.record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;

    fn collect(s: ExternalSorter) -> Vec<Record> {
        s.finish()
            .expect("finish")
            .collect::<io::Result<Vec<_>>>()
            .expect("records")
    }

    #[test]
    fn sorts_by_key_secondary_and_payload() {
        let s = ExternalSorter::new(None, 64 << 20).expect("sorter");
        {
            let mut w = s.writer();
            w.push(2, 0, b"b").expect("push");
            w.push(1, 5, b"z").expect("push");
            w.push(1, 5, b"a").expect("push");
            w.push(1, 1, b"q").expect("push");
        }
        let got: Vec<_> = collect(s)
            .into_iter()
            .map(|r| (r.key, r.secondary, r.payload))
            .collect();
        assert_eq!(
            got,
            [
                (1, 1, b"q".to_vec()),
                (1, 5, b"a".to_vec()),
                (1, 5, b"z".to_vec()),
                (2, 0, b"b".to_vec())
            ]
        );
    }

    #[test]
    fn parallel_spilling_sort_is_deterministic_and_complete() {
        let run = || {
            // Tiny budget forces many spills and a multi-round merge.
            let s = ExternalSorter::new(None, 0).expect("sorter");
            (0..64u64).into_par_iter().for_each(|t| {
                let mut w = s.writer();
                for i in 0..20_000u64 {
                    let key = (i * 7919 + t * 104_729) % 5_000;
                    let payload = [(t as u8), (i % 251) as u8, 0xAB].repeat(200);
                    w.push(key, i % 3, &payload).expect("push");
                }
            });
            assert!(s.spilled_bytes() > 0, "budget should force spilling");
            let dir = s.temp_dir().to_path_buf();
            let out = collect(s);
            assert!(!dir.exists(), "temp dir removed after the merge");
            out
        };
        let a = run();
        assert_eq!(a.len(), 64 * 20_000);
        assert!(
            a.windows(2)
                .all(|w| (w[0].key, w[0].secondary, &w[0].payload)
                    <= (w[1].key, w[1].secondary, &w[1].payload))
        );
        let b = run();
        assert!(a == b, "same input, same output regardless of scheduling");
    }

    #[test]
    fn multi_round_merge() {
        let mut s = ExternalSorter::new(None, 0).expect("sorter");
        s.fan_in = 3;
        s.chunk_bytes = 4096;
        {
            let mut w = s.writer();
            for i in (0..50_000u64).rev() {
                w.push(i % 997, i, &i.to_le_bytes()).expect("push");
            }
        }
        assert!(
            s.files.lock().expect("lock").len() > 9,
            "needs several merge rounds"
        );
        let out = collect(s);
        assert_eq!(out.len(), 50_000);
        assert!(
            out.windows(2)
                .all(|w| (w[0].key, w[0].secondary) <= (w[1].key, w[1].secondary))
        );
    }

    #[test]
    fn truncated_spill_file_is_an_error_not_a_short_stream() {
        let s = ExternalSorter::new(None, 0).expect("sorter");
        {
            let mut w = s.writer();
            for i in 0..2_000_000u64 {
                w.push(i, 0, &[1, 2, 3, 4]).expect("push");
            }
        }
        let first = s
            .files
            .lock()
            .expect("lock")
            .first()
            .cloned()
            .expect("spilled");
        let len = std::fs::metadata(&first).expect("meta").len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&first)
            .expect("open")
            .set_len(len - 3)
            .expect("truncate");
        let results: Vec<_> = s.finish().expect("finish").collect();
        assert!(
            results.iter().any(|r| r.is_err()),
            "truncation must surface as an error"
        );
    }

    #[test]
    fn temp_dir_is_removed_when_sorter_is_dropped_unfinished() {
        let s = ExternalSorter::new(None, 0).expect("sorter");
        {
            let mut w = s.writer();
            for i in 0..1_000_000u64 {
                w.push(i, 0, &[0; 8]).expect("push");
            }
        }
        let dir = s.temp_dir().to_path_buf();
        assert!(dir.exists());
        drop(s);
        assert!(!dir.exists());
    }

    #[test]
    fn empty_sorter() {
        let s = ExternalSorter::new(None, 1 << 20).expect("sorter");
        assert!(collect(s).is_empty());
    }
}
