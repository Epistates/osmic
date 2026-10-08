//! Parallel external sort for variable-length records.
//!
//! Many threads append records through [`SortWriter`]s, each filling its own
//! in-memory chunk. A full chunk is sorted by the thread that filled it and
//! spilled to a file in a private temporary directory, together with a small
//! index of where key runs start.
//!
//! [`ExternalSorter::finish`] does not merge. It cuts the key space into
//! [partitions](SortedRuns::partition) of roughly equal size, using the
//! spill indexes, so each partition can be read from every file (one range
//! per file) and sorted in memory independently — on as many threads as are
//! available — while a key never spans two partitions. Small inputs never
//! touch the disk.
//!
//! Ordering is total — `(key, secondary, payload bytes)` — so output is
//! identical regardless of how work was split across threads. The
//! temporary directory is removed when the sorter or its output is dropped,
//! including on error.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use tempfile::TempDir;

const IO_BUFFER: usize = 1 << 20;
const HEADER: usize = 20;
/// Spill files are indexed at the first key run after every this many bytes.
const INDEX_STRIDE: u64 = 16 << 10;

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

/// A sorted spill file and the offsets of key runs within it.
#[derive(Debug)]
struct Spill {
    path: PathBuf,
    len: u64,
    /// `(key, offset)` of the first record of a key run, ascending.
    index: Vec<(u64, u64)>,
}

/// Parallel external sorter. See the module docs.
pub struct ExternalSorter {
    dir: TempDir,
    chunk_bytes: usize,
    memory_budget: usize,
    threads: usize,
    pool: Mutex<Vec<Chunk>>,
    spills: Mutex<Vec<Spill>>,
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
    /// `memory_budget / (2 × threads)` (clamped to 16 MiB–512 MiB). The
    /// same budget sizes the partitions read back by [`finish`](Self::finish).
    pub fn new(temp_parent: Option<&Path>, memory_budget: usize) -> io::Result<Self> {
        let prefix = format!("{}sort-", osmic_core::fs::temp_file_prefix());
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix);
        let dir = match temp_parent {
            Some(p) => builder.tempdir_in(p)?,
            None => builder.tempdir()?,
        };
        let threads = rayon::current_num_threads().max(1);
        let chunk_bytes = (memory_budget / (2 * threads)).clamp(16 << 20, 512 << 20);
        Ok(Self {
            dir,
            chunk_bytes,
            memory_budget,
            threads,
            pool: Mutex::new(Vec::new()),
            spills: Mutex::new(Vec::new()),
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
            pushed: 0,
        }
    }

    /// Records added by writers dropped so far.
    pub fn records(&self) -> u64 {
        self.records.load(AtomicOrdering::Relaxed)
    }

    /// Bytes written to temporary files so far.
    pub fn spilled_bytes(&self) -> u64 {
        self.spilled_bytes.load(AtomicOrdering::Relaxed)
    }

    fn spill(&self, chunk: &mut Chunk) -> io::Result<()> {
        chunk.sort();
        let n = self.next_file.fetch_add(1, AtomicOrdering::Relaxed);
        let path = self.dir.path().join(format!("chunk-{n:06}.bin"));
        let mut w = BufWriter::with_capacity(IO_BUFFER, File::create(&path)?);
        let mut index = Vec::new();
        let mut offset = 0u64;
        let mut since_index = INDEX_STRIDE;
        let mut prev_key = None;
        for e in &chunk.entries {
            if since_index >= INDEX_STRIDE && prev_key != Some(e.key) {
                index.push((e.key, offset));
                since_index = 0;
            }
            write_record(&mut w, e.key, e.secondary, chunk.payload(e))?;
            let n = (HEADER + e.len as usize) as u64;
            offset += n;
            since_index += n;
            prev_key = Some(e.key);
        }
        w.flush()?;
        chunk.clear();
        self.spilled_bytes
            .fetch_add(offset, AtomicOrdering::Relaxed);
        self.spills
            .lock()
            .map_err(|_| io::Error::other("sorter file list poisoned"))?
            .push(Spill {
                path,
                len: offset,
                index,
            });
        Ok(())
    }

    /// Stop accepting records and plan the read-back. All writers must
    /// have been dropped.
    pub fn finish(self) -> io::Result<SortedRuns> {
        let mut chunks = self
            .pool
            .into_inner()
            .map_err(|_| io::Error::other("sorter pool poisoned"))?;
        let mut spills = self
            .spills
            .into_inner()
            .map_err(|_| io::Error::other("sorter file list poisoned"))?;
        spills.sort_by(|a, b| a.path.cmp(&b.path));
        chunks.retain(|c| !c.entries.is_empty());
        for c in &mut chunks {
            c.sort();
        }

        // Byte-weighted samples at key-run starts, from the spill indexes
        // and the chunks still in memory.
        let mut samples: Vec<(u64, u64)> = Vec::new();
        for s in &spills {
            for (j, &(key, offset)) in s.index.iter().enumerate() {
                let end = s.index.get(j + 1).map_or(s.len, |&(_, o)| o);
                samples.push((key, end - offset));
            }
        }
        for c in &chunks {
            let mut current: Option<(u64, u64)> = None;
            for e in &c.entries {
                let n = (HEADER + e.len as usize) as u64;
                match &mut current {
                    Some((key, bytes)) if *bytes < INDEX_STRIDE || *key == e.key => *bytes += n,
                    _ => samples.extend(current.replace((e.key, n))),
                }
            }
            samples.extend(current);
        }
        samples.sort_unstable_by_key(|&(key, _)| key);

        // Cut at sample keys so no key spans two partitions. Reading a
        // partition may also touch up to two index strides per file outside
        // its range, so partitions grow with the number of files. A
        // partition in flight holds its records plus their decoded and
        // encoded forms, about twice its size; the window keeps that within
        // the budget while giving every thread work.
        let budget = self.memory_budget as u64;
        let target = (budget / (4 * self.threads as u64))
            .clamp(4 << 20, 64 << 20)
            .max(spills.len() as u64 * 8 * INDEX_STRIDE);
        let window = usize::try_from(budget / (2 * target))
            .unwrap_or(usize::MAX)
            .clamp(2, 2 * self.threads);
        let mut bounds: Vec<u64> = Vec::new();
        let mut acc = 0u64;
        for (key, bytes) in samples {
            if acc >= target && bounds.last().is_none_or(|&b| key > b) {
                bounds.push(key);
                acc = 0;
            }
            acc += bytes;
        }
        Ok(SortedRuns {
            spills,
            chunks,
            bounds,
            window,
            _dir: self.dir,
        })
    }
}

/// A per-thread handle for adding records. Returns its buffer to the
/// sorter's pool when dropped.
pub struct SortWriter<'a> {
    sorter: &'a ExternalSorter,
    chunk: Option<Chunk>,
    pushed: u64,
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
        self.pushed += 1;
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
        self.sorter
            .records
            .fetch_add(self.pushed, AtomicOrdering::Relaxed);
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

fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// Everything a sorter received, ready to be read back in key order one
/// partition at a time. Partitions are independent: read them from any
/// number of threads, in any order.
pub struct SortedRuns {
    spills: Vec<Spill>,
    chunks: Vec<Chunk>,
    /// Partition `i` holds keys in `[bounds[i - 1], bounds[i])`.
    bounds: Vec<u64>,
    window: usize,
    _dir: TempDir,
}

impl std::fmt::Debug for SortedRuns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SortedRuns")
            .field("spill_files", &self.spills.len())
            .field("memory_chunks", &self.chunks.len())
            .field("partitions", &self.partition_count())
            .finish()
    }
}

/// Where a partition entry's payload lives.
#[derive(Debug, Clone, Copy)]
enum Src {
    Read(u32),
    Chunk(u32),
}

#[derive(Debug, Clone, Copy)]
struct PartEntry {
    key: u64,
    secondary: u64,
    src: Src,
    offset: usize,
    len: u32,
}

fn payload_of<'a>(reads: &'a [Vec<u8>], chunks: &'a [Chunk], e: &PartEntry) -> &'a [u8] {
    let data = match e.src {
        Src::Read(i) => &reads[i as usize][..],
        Src::Chunk(i) => &chunks[i as usize].data[..],
    };
    &data[e.offset..e.offset + e.len as usize]
}

impl SortedRuns {
    /// Number of partitions (at least one).
    pub fn partition_count(&self) -> usize {
        self.bounds.len() + 1
    }

    /// How many partitions to hold at once to stay within the sorter's
    /// memory budget while keeping every thread busy.
    pub fn window(&self) -> usize {
        self.window
    }

    /// The records of partition `i`, sorted. Partition `i` holds every
    /// record whose key is above those of partition `i - 1` and below those
    /// of partition `i + 1`.
    ///
    /// # Errors
    ///
    /// I/O errors, including spill files that were truncated or corrupted.
    pub fn partition(&self, i: usize) -> io::Result<Partition<'_>> {
        let lo = if i == 0 { 0 } else { self.bounds[i - 1] };
        let hi = self.bounds.get(i).copied();
        let in_range = |key: u64| key >= lo && hi.is_none_or(|h| key < h);

        let mut reads: Vec<Vec<u8>> = Vec::new();
        let mut entries: Vec<PartEntry> = Vec::new();
        for s in &self.spills {
            // Records before the last run start at or below `lo` are below
            // `lo`; records from the first run start at or above `hi` on are
            // at or above `hi`.
            let start = match s.index.partition_point(|&(k, _)| k <= lo) {
                0 => 0,
                j => s.index[j - 1].1,
            };
            let end = hi.map_or(s.len, |h| {
                let j = s.index.partition_point(|&(k, _)| k < h);
                s.index.get(j).map_or(s.len, |&(_, o)| o)
            });
            if end <= start {
                continue;
            }
            let mut buf = vec![0u8; usize::try_from(end - start).map_err(io::Error::other)?];
            let mut file = File::open(&s.path)?;
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut buf).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("reading sort spill {}: {e}", s.path.display()),
                )
            })?;
            let src = Src::Read(u32::try_from(reads.len()).map_err(io::Error::other)?);
            let mut pos = 0usize;
            while pos < buf.len() {
                let corrupt = || {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("corrupt sort spill {}", s.path.display()),
                    )
                };
                let header = buf.get(pos..pos + HEADER).ok_or_else(corrupt)?;
                let key = le_u64(&header[0..8]);
                let secondary = le_u64(&header[8..16]);
                let len = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
                let offset = pos + HEADER;
                pos = offset + len as usize;
                if pos > buf.len() {
                    return Err(corrupt());
                }
                if in_range(key) {
                    entries.push(PartEntry {
                        key,
                        secondary,
                        src,
                        offset,
                        len,
                    });
                }
            }
            reads.push(buf);
        }
        for (ci, c) in self.chunks.iter().enumerate() {
            let a = c.entries.partition_point(|e| e.key < lo);
            let b = hi.map_or(c.entries.len(), |h| {
                c.entries.partition_point(|e| e.key < h)
            });
            let src = Src::Chunk(u32::try_from(ci).map_err(io::Error::other)?);
            entries.extend(c.entries[a..b].iter().map(|e| PartEntry {
                key: e.key,
                secondary: e.secondary,
                src,
                offset: e.offset,
                len: e.len,
            }));
        }
        let chunks = &self.chunks[..];
        entries.sort_unstable_by(|a, b| {
            (a.key, a.secondary)
                .cmp(&(b.key, b.secondary))
                .then_with(|| payload_of(&reads, chunks, a).cmp(payload_of(&reads, chunks, b)))
        });
        Ok(Partition {
            reads,
            chunks,
            entries,
        })
    }

    /// Every record in order, reading one partition at a time.
    pub fn records(&self) -> impl Iterator<Item = io::Result<Record>> + '_ {
        let mut failed = false;
        (0..self.partition_count())
            .map_while(move |i| {
                if failed {
                    return None;
                }
                let part = self.partition(i);
                failed = part.is_err();
                Some(part.map(|p| p.records().collect::<Vec<_>>()))
            })
            .flat_map(|part| match part {
                Ok(records) => records.into_iter().map(Ok).collect::<Vec<_>>(),
                Err(e) => vec![Err(e)],
            })
    }
}

/// The sorted records of one partition.
pub struct Partition<'a> {
    reads: Vec<Vec<u8>>,
    chunks: &'a [Chunk],
    entries: Vec<PartEntry>,
}

impl Partition<'_> {
    /// Number of records.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Records grouped by key: `(key, [(secondary, payload)])`, each group
    /// sorted by secondary key then payload.
    pub fn groups(&self) -> impl Iterator<Item = (u64, impl Iterator<Item = (u64, &[u8])>)> {
        self.entries.chunk_by(|a, b| a.key == b.key).map(|group| {
            (
                group[0].key,
                group
                    .iter()
                    .map(|e| (e.secondary, payload_of(&self.reads, self.chunks, e))),
            )
        })
    }

    /// Owned copies of the records, in order.
    pub fn records(&self) -> impl Iterator<Item = Record> + '_ {
        self.entries.iter().map(|e| Record {
            key: e.key,
            secondary: e.secondary,
            payload: payload_of(&self.reads, self.chunks, e).to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;

    fn collect(s: ExternalSorter) -> Vec<Record> {
        s.finish()
            .expect("finish")
            .records()
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
        assert_eq!(s.records(), 4);
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
            // Tiny budget forces many spills and many partitions.
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
            let runs = s.finish().expect("finish");
            assert!(runs.partition_count() > 10, "{runs:?}");
            let out = runs
                .records()
                .collect::<io::Result<Vec<_>>>()
                .expect("records");
            drop(runs);
            assert!(!dir.exists(), "temp dir removed with the output");
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
    fn partitions_split_between_keys_and_cover_everything() {
        let mut s = ExternalSorter::new(None, 0).expect("sorter");
        s.chunk_bytes = 1 << 20;
        (0..8u64).into_par_iter().for_each(|t| {
            let mut w = s.writer();
            for i in 0..60_000u64 {
                // Heavily repeated keys, so runs straddle index strides.
                w.push((i * 31 + t) % 700, t, &[t as u8; 40]).expect("push");
            }
        });
        // Leave some records in memory alongside the spill files.
        {
            let mut w = s.writer();
            for i in 0..1_000u64 {
                w.push(i % 700, 99, b"mem").expect("push");
            }
        }
        let runs = s.finish().expect("finish");
        assert!(!runs.chunks.is_empty() && !runs.spills.is_empty());
        assert!(runs.partition_count() > 2, "{runs:?}");
        let mut total = 0;
        let mut last_key = None;
        for i in 0..runs.partition_count() {
            let part = runs.partition(i).expect("partition");
            total += part.len();
            for (key, records) in part.groups() {
                assert!(
                    last_key.is_none_or(|k| key > k),
                    "key {key} split or out of order"
                );
                last_key = Some(key);
                let secondaries: Vec<u64> = records.map(|(s, _)| s).collect();
                assert!(secondaries.is_sorted());
            }
        }
        assert_eq!(total, 8 * 60_000 + 1_000);
    }

    #[test]
    fn many_spill_files_need_no_extra_file_handles() {
        let mut s = ExternalSorter::new(None, 0).expect("sorter");
        s.chunk_bytes = 4096;
        {
            let mut w = s.writer();
            for i in (0..50_000u64).rev() {
                w.push(i % 997, i, &i.to_le_bytes()).expect("push");
            }
        }
        // More files than the default macOS open-file limit (256).
        assert!(s.spills.lock().expect("lock").len() > 300);
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
            .spills
            .lock()
            .expect("lock")
            .first()
            .map(|s| s.path.clone())
            .expect("spilled");
        let len = std::fs::metadata(&first).expect("meta").len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&first)
            .expect("open")
            .set_len(len - 3)
            .expect("truncate");
        let runs = s.finish().expect("finish");
        let results: Vec<_> = runs.records().collect();
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
        let runs = s.finish().expect("finish");
        assert_eq!(runs.partition_count(), 1);
        assert!(runs.partition(0).expect("partition").is_empty());
        assert_eq!(runs.records().count(), 0);
    }
}
