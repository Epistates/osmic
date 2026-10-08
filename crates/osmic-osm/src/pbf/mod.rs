//! OSM PBF input (via `osmpbf`) and output.
//!
//! [`read_header`] inspects and validates a file's header block;
//! [`par_blocks`] decodes data blocks in parallel and returns per-block
//! results tagged with their position in the file, so callers can produce
//! deterministic output regardless of thread scheduling. [`PbfWriter`]
//! writes PBF files (replication output, extracts, test fixtures).

mod writer;

pub use writer::{ElementMeta, MAX_BLOCK_BYTES, PbfWriter, PbfWriterOptions};

use std::borrow::Cow;
use std::path::Path;

use osmpbf::{BlobDecode, BlobReader, PrimitiveBlock};
use rayon::prelude::*;

use osmic_core::{BBox, FixedCoord};

use crate::error::OsmError;

/// Required features osmic understands.
const SUPPORTED_REQUIRED_FEATURES: &[&str] = &["OsmSchema-V0.6", "DenseNodes"];

/// Contents of a PBF file's `OSMHeader` block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PbfHeader {
    pub required_features: Vec<String>,
    pub optional_features: Vec<String>,
    pub bbox: Option<BBox>,
    pub writing_program: Option<String>,
    pub source: Option<String>,
    pub replication_timestamp: Option<i64>,
    pub replication_sequence: Option<i64>,
    pub replication_base_url: Option<String>,
}

impl PbfHeader {
    /// Elements are sorted by type (nodes, ways, relations) then id.
    pub fn is_sorted(&self) -> bool {
        self.optional_features
            .iter()
            .any(|f| f == "Sort.Type_then_ID")
    }

    /// Ways carry their node coordinates (`osmium add-locations-to-ways`),
    /// so no node index is needed to build way geometry.
    pub fn has_locations_on_ways(&self) -> bool {
        self.optional_features
            .iter()
            .chain(&self.required_features)
            .any(|f| f == "LocationsOnWays")
    }
}

/// A location from osmpbf's nanodegree values, or `None` if it does not
/// fit OSM's 1e-7° grid or lies outside the valid range.
///
/// osmpbf's own `decimicro_*` accessors truncate with `as i32`, which wraps
/// far out-of-range values back into valid-looking ones; osmium marks a
/// missing way-node location as `i32::MAX`, which is out of range too.
pub fn location(nano_lon: i64, nano_lat: i64) -> Option<FixedCoord> {
    let lon = i32::try_from(nano_lon / 100).ok()?;
    let lat = i32::try_from(nano_lat / 100).ok()?;
    let c = FixedCoord::new(lon, lat);
    c.is_valid().then_some(c)
}

/// A block's string table, decoded once.
///
/// osmpbf's tag iterators validate UTF-8 on every access and silently stop
/// at the first invalid string, dropping the element's remaining tags.
/// Decoding the table once is cheaper when tags are read more than once,
/// and invalid sequences become U+FFFD so every tag is kept.
pub struct StringTable<'a> {
    strings: Vec<Cow<'a, str>>,
}

impl<'a> StringTable<'a> {
    pub fn new(block: &'a PrimitiveBlock) -> Self {
        Self {
            strings: block
                .raw_stringtable()
                .iter()
                .map(|s| String::from_utf8_lossy(s))
                .collect(),
        }
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        self.strings.get(index).map(|s| &**s)
    }

    /// Tags from `(key, value)` string-table indices (as returned by
    /// osmpbf's `raw_tags`); pairs referring outside the table are skipped.
    pub fn tags<'s, K, I>(&'s self, raw: I) -> impl Iterator<Item = (&'s str, &'s str)> + Clone
    where
        K: TryInto<usize>,
        I: Iterator<Item = (K, K)> + Clone + 's,
    {
        raw.filter_map(|(k, v)| {
            let (k, v) = (k.try_into().ok()?, v.try_into().ok()?);
            Some((self.get(k)?, self.get(v)?))
        })
    }
}

/// Read and validate the header of a PBF file.
///
/// Fails for files that need features osmic does not implement — notably
/// OSM history files (`HistoricalInformation`), where several versions of
/// each object would otherwise be mixed into one dataset.
pub fn read_header(path: &Path) -> Result<PbfHeader, OsmError> {
    let reader = BlobReader::from_path(path).map_err(|e| OsmError::pbf(path, e))?;
    for blob in reader {
        let blob = blob.map_err(|e| OsmError::pbf(path, e))?;
        match blob.decode().map_err(|e| OsmError::pbf(path, e))? {
            BlobDecode::OsmHeader(h) => {
                let header = PbfHeader {
                    required_features: h.required_features().to_vec(),
                    optional_features: h.optional_features().to_vec(),
                    bbox: h
                        .bbox()
                        .map(|b| BBox::new(b.left, b.bottom, b.right, b.top)),
                    writing_program: h.writing_program().map(str::to_owned),
                    source: h.source().map(str::to_owned),
                    replication_timestamp: h.osmosis_replication_timestamp(),
                    replication_sequence: h.osmosis_replication_sequence_number(),
                    replication_base_url: h.osmosis_replication_base_url().map(str::to_owned),
                };
                if let Some(unsupported) = header.required_features.iter().find(|f| {
                    !SUPPORTED_REQUIRED_FEATURES.contains(&f.as_str()) && *f != "LocationsOnWays"
                }) {
                    return Err(OsmError::UnsupportedFeature {
                        path: path.to_path_buf(),
                        feature: unsupported.clone(),
                    });
                }
                return Ok(header);
            }
            BlobDecode::OsmData(_) => break,
            BlobDecode::Unknown(_) => {}
        }
    }
    Err(OsmError::MissingHeader(path.to_path_buf()))
}

/// Decode every data block of `path` in parallel and apply `f` to it.
///
/// Returns `(block_sequence, result)` pairs sorted by sequence (file
/// order). Stops at the first error.
pub fn par_blocks<T, F>(path: &Path, f: F) -> Result<Vec<(u64, T)>, OsmError>
where
    T: Send,
    F: Fn(&PrimitiveBlock) -> Result<T, OsmError> + Sync,
{
    let reader = BlobReader::from_path(path).map_err(|e| OsmError::pbf(path, e))?;
    let mut results: Vec<(u64, T)> = reader
        .enumerate()
        .par_bridge()
        .map(|(seq, blob)| -> Result<Option<(u64, T)>, OsmError> {
            let blob = blob.map_err(|e| OsmError::pbf(path, e))?;
            let decoded = blob
                .decode()
                .map_err(|e| OsmError::block(path, seq as u64, e))?;
            match decoded {
                BlobDecode::OsmData(block) => Ok(Some((seq as u64, f(&block)?))),
                BlobDecode::OsmHeader(_) | BlobDecode::Unknown(_) => Ok(None),
            }
        })
        .filter_map(Result::transpose)
        .collect::<Result<_, _>>()?;
    results.sort_unstable_by_key(|(seq, _)| *seq);
    Ok(results)
}
