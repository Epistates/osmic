//! Opening PMTiles archives for reading, safely.
//!
//! [`open`] validates the 127-byte header before handing the file to the
//! `pmtiles` reader: that reader slices its initial read using the header's
//! root-directory offset and length without checking them, so a malformed
//! file would panic instead of failing. Every section the header declares
//! must lie inside the file. The reader caches decoded leaf directories in a
//! bounded cache, so serving a large archive does not re-read and re-inflate
//! a leaf directory for every tile. [`decompress_tile`] inflates tile data
//! with a size cap.

use std::borrow::Cow;
use std::io::Read;
use std::path::{Path, PathBuf};

use pmtiles::{AsyncPmTilesReader, Compression, MmapBackend, MokaCache, PmtError};

/// A PMTiles archive reader with a bounded leaf-directory cache.
pub type ArchiveReader = AsyncPmTilesReader<MmapBackend, MokaCache>;

/// Leaf directories cached by default.
pub const DEFAULT_DIRECTORY_CACHE: u64 = 256;

const HEADER_LEN: usize = 127;
/// The root directory must lie within the first 16 KiB (PMTiles v3).
const MAX_INITIAL_BYTES: u64 = 16_384;

/// Why an archive could not be opened.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenArchiveError {
    #[error("PMTiles archive not found: {}", .0.display())]
    NotFound(PathBuf),
    #[error("cannot read PMTiles archive {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a valid PMTiles v3 archive: {reason}", path.display())]
    Invalid { path: PathBuf, reason: String },
    #[error("failed to read PMTiles archive {}", path.display())]
    Pmtiles {
        path: PathBuf,
        #[source]
        source: PmtError,
    },
}

impl OpenArchiveError {
    /// Wrap an error from a later read of the archive at `path`.
    pub fn read(path: &Path, source: PmtError) -> Self {
        Self::Pmtiles {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Open `path` after validating its header, caching up to
/// `directory_cache` decoded leaf directories.
///
/// The file is memory-mapped: it must not be truncated or rewritten while
/// the reader (or tile data read from it) is alive. Replace archives by
/// renaming a new file over the old one.
///
/// # Errors
///
/// [`OpenArchiveError`] if the file is missing, unreadable, not a PMTiles
/// v3 archive, or declares sections outside the file.
pub async fn open(path: &Path, directory_cache: u64) -> Result<ArchiveReader, OpenArchiveError> {
    let io = |source| OpenArchiveError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file_len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(OpenArchiveError::NotFound(path.to_path_buf()));
        }
        Err(e) => return Err(io(e)),
    };
    let mut header = [0u8; HEADER_LEN];
    let mut file = std::fs::File::open(path).map_err(io)?;
    let read = read_up_to(&mut file, &mut header).map_err(io)?;
    validate_header(&header[..read], file_len).map_err(|reason| OpenArchiveError::Invalid {
        path: path.to_path_buf(),
        reason,
    })?;
    let backend = MmapBackend::try_from(path)
        .await
        .map_err(|e| OpenArchiveError::read(path, e))?;
    let cache = MokaCache {
        cache: moka::future::Cache::new(directory_cache.max(1)),
    };
    AsyncPmTilesReader::try_from_cached_source(backend, cache)
        .await
        .map_err(|e| OpenArchiveError::read(path, e))
}

/// Default upper bound for a decompressed tile, a guard against
/// decompression bombs.
pub const MAX_DECOMPRESSED_TILE: u64 = 64 * 1024 * 1024;

/// Why a tile could not be decompressed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DecompressError {
    #[error("tile decompresses to more than {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("{0:?} tile compression is not supported")]
    Unsupported(Compression),
    #[error("corrupt compressed tile")]
    Corrupt(#[source] std::io::Error),
}

/// Decompress the raw bytes of a tile stored with `compression`, refusing
/// to produce more than `limit` bytes.
///
/// Unlike `AsyncPmTilesReader::get_tile_decompressed`, a crafted archive
/// cannot make this allocate without bound. Uncompressed tiles are borrowed
/// (and subject to the same limit).
///
/// # Errors
///
/// [`DecompressError::TooLarge`] past `limit`, [`DecompressError::Corrupt`]
/// for invalid compressed data, [`DecompressError::Unsupported`] for
/// brotli, zstd and unknown compression.
pub fn decompress_tile(
    raw: &[u8],
    compression: Compression,
    limit: u64,
) -> Result<Cow<'_, [u8]>, DecompressError> {
    let too_large = || DecompressError::TooLarge { limit };
    match compression {
        Compression::None if raw.len() as u64 > limit => Err(too_large()),
        Compression::None => Ok(Cow::Borrowed(raw)),
        Compression::Gzip => {
            // A tile rarely inflates by more than ~10x; cap the initial
            // reservation so a tiny bomb cannot reserve `limit` up front.
            let hint = raw.len().saturating_mul(4).min(limit as usize);
            let mut out = Vec::with_capacity(hint);
            flate2::read::GzDecoder::new(raw)
                .take(limit.saturating_add(1))
                .read_to_end(&mut out)
                .map_err(DecompressError::Corrupt)?;
            if out.len() as u64 > limit {
                return Err(too_large());
            }
            Ok(Cow::Owned(out))
        }
        other => Err(DecompressError::Unsupported(other)),
    }
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Check a PMTiles v3 header against the file length: magic and version,
/// a root directory inside the first 16 KiB and after the header, and every
/// other section (metadata, leaf directories, tile data) inside the file.
pub fn validate_header(header: &[u8], file_len: u64) -> Result<(), String> {
    if header.len() < HEADER_LEN {
        return Err(format!(
            "file is {} bytes, shorter than the header",
            header.len()
        ));
    }
    if &header[..7] != b"PMTiles" {
        return Err("bad magic bytes".into());
    }
    if header[7] != 3 {
        return Err(format!("unsupported version {}", header[7]));
    }
    let u64_at = |at: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&header[at..at + 8]);
        u64::from_le_bytes(b)
    };
    let section = |name: &str, at: usize, limit: u64| -> Result<(u64, u64), String> {
        let (offset, len) = (u64_at(at), u64_at(at + 8));
        match offset.checked_add(len) {
            Some(end) if end <= limit => Ok((offset, end)),
            _ => Err(format!(
                "{name} section {offset}+{len} extends past {limit} bytes"
            )),
        }
    };
    let (root_offset, _) = section("root directory", 8, file_len.min(MAX_INITIAL_BYTES))?;
    if root_offset < HEADER_LEN as u64 {
        return Err(format!(
            "root directory offset {root_offset} overlaps the header"
        ));
    }
    section("metadata", 24, file_len)?;
    section("leaf directory", 40, file_len)?;
    section("tile data", 56, file_len)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(fields: [u64; 8]) -> Vec<u8> {
        let mut h = b"PMTiles\x03".to_vec();
        for f in fields {
            h.extend_from_slice(&f.to_le_bytes());
        }
        h.resize(HEADER_LEN, 0);
        h
    }

    #[test]
    fn accepts_sections_inside_the_file() {
        let h = header([127, 100, 227, 50, 277, 0, 277, 1000]);
        assert_eq!(validate_header(&h, 1277), Ok(()));
    }

    #[test]
    fn rejects_malformed_headers() {
        let ok = [127, 100, 227, 50, 277, 0, 277, 1000];
        let cases: [(Vec<u8>, &str); 7] = [
            (header(ok)[..100].to_vec(), "shorter"),
            (
                {
                    let mut h = header(ok);
                    h[0] = b'X';
                    h
                },
                "magic",
            ),
            (
                {
                    let mut h = header(ok);
                    h[7] = 2;
                    h
                },
                "version",
            ),
            (header([10, 100, 227, 50, 277, 0, 277, 1000]), "overlaps"),
            (header([127, 20_000, 227, 50, 277, 0, 277, 1000]), "root"),
            (
                header([127, 100, u64::MAX, 2, 277, 0, 277, 1000]),
                "metadata",
            ),
            (
                header([127, 100, 227, 50, 277, 0, 277, 10_000]),
                "tile data",
            ),
        ];
        for (h, expect) in cases {
            let err = validate_header(&h, 1277).expect_err(expect);
            assert!(err.contains(expect), "{expect}: {err}");
        }
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).expect("gzip");
        enc.finish().expect("gzip")
    }

    #[test]
    fn decompression_is_capped() {
        let tile = vec![7u8; 1000];
        let packed = gzip(&tile);
        assert_eq!(
            decompress_tile(&packed, Compression::Gzip, 1000)
                .expect("fits")
                .as_ref(),
            &tile[..]
        );
        assert!(matches!(
            decompress_tile(&packed, Compression::Gzip, 999),
            Err(DecompressError::TooLarge { limit: 999 })
        ));
        // A bomb: 64 MiB of zeros packs into ~64 KiB.
        let bomb = gzip(&vec![0u8; 64 << 20]);
        assert!(bomb.len() < 1 << 20);
        assert!(matches!(
            decompress_tile(&bomb, Compression::Gzip, 1 << 20),
            Err(DecompressError::TooLarge { .. })
        ));
        assert!(matches!(
            decompress_tile(&tile, Compression::None, 999),
            Err(DecompressError::TooLarge { .. })
        ));
        assert!(matches!(
            decompress_tile(&tile, Compression::None, 1000),
            Ok(Cow::Borrowed(_))
        ));
    }

    #[test]
    fn corrupt_and_unsupported_tiles_are_errors() {
        assert!(matches!(
            decompress_tile(b"not gzip at all", Compression::Gzip, 1 << 20),
            Err(DecompressError::Corrupt(_))
        ));
        for c in [Compression::Brotli, Compression::Zstd, Compression::Unknown] {
            assert!(matches!(
                decompress_tile(b"x", c, 1 << 20),
                Err(DecompressError::Unsupported(_))
            ));
        }
    }

    #[cfg(feature = "native")]
    #[tokio::test]
    async fn reads_tiles_through_the_cache() {
        use crate::assemble::TileCompression;
        use crate::encode::TileFormat;
        use crate::pmtiles::{ArchiveOptions, PmTilesArchive};
        use osmic_core::{BBox, TileCoord};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.pmtiles");
        let options = ArchiveOptions {
            format: TileFormat::Mvt,
            compression: TileCompression::None,
            bounds: BBox::world(),
            min_zoom: 0,
            max_zoom: 2,
            metadata: serde_json::json!({}),
            overwrite: false,
        };
        let mut archive = PmTilesArchive::create(&path, &options).expect("create");
        let coords = [(0, 0, 0), (1, 1, 0), (2, 3, 3)];
        for (z, x, y) in coords {
            let coord = TileCoord::try_new(x, y, z).expect("coord");
            archive
                .add_tile(coord, &[z, x as u8, y as u8])
                .expect("add");
        }
        archive.finalize().expect("finalize");

        let reader = open(&path, 4).await.expect("open");
        for (z, x, y) in coords {
            let tile = reader
                .get_tile(pmtiles::TileCoord::new(z, x, y).expect("coord"))
                .await
                .expect("read")
                .expect("present");
            assert_eq!(&tile[..], [z, x as u8, y as u8]);
        }
        let absent = pmtiles::TileCoord::new(2, 0, 0).expect("coord");
        assert!(reader.get_tile(absent).await.expect("read").is_none());
    }

    #[tokio::test]
    async fn missing_and_invalid_files_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("none.pmtiles");
        assert!(matches!(
            open(&missing, 8).await,
            Err(OpenArchiveError::NotFound(_))
        ));
        let bogus = dir.path().join("bogus.pmtiles");
        std::fs::write(&bogus, header([10, 100, 0, 0, 0, 0, 0, 0])).expect("write");
        assert!(matches!(
            open(&bogus, 8).await,
            Err(OpenArchiveError::Invalid { .. })
        ));
    }
}
