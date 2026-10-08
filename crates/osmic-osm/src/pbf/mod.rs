//! OSM PBF input (via `osmpbf`) and output.
//!
//! [`read_header`] inspects and validates a file's header block;
//! [`par_blocks`] decodes data blocks in parallel and returns per-block
//! results tagged with their position in the file, so callers can produce
//! deterministic output regardless of thread scheduling. [`PbfWriter`]
//! writes PBF files (used for test fixtures and extract output).

mod writer;

pub use writer::{PbfWriter, PbfWriterOptions};

use std::path::Path;

use osmpbf::{BlobDecode, BlobReader, PrimitiveBlock};
use rayon::prelude::*;

use osmic_core::BBox;

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

/// Decode blocks in parallel, `window` at a time, and hand the results of
/// `decode` to `consume` in file order. At most `window` decoded blocks are
/// held in memory, so this streams files of any size in order (unlike
/// [`par_blocks`], which collects every result).
pub fn for_each_block_ordered<T, D, C>(
    path: &Path,
    window: usize,
    decode: D,
    mut consume: C,
) -> Result<(), OsmError>
where
    T: Send,
    D: Fn(&PrimitiveBlock) -> Result<T, OsmError> + Sync,
    C: FnMut(T) -> Result<(), OsmError>,
{
    let mut reader = BlobReader::from_path(path)
        .map_err(|e| OsmError::pbf(path, e))?
        .enumerate();
    let window = window.max(1);
    loop {
        let mut blobs = Vec::with_capacity(window);
        for (seq, blob) in reader.by_ref().take(window) {
            blobs.push((seq as u64, blob.map_err(|e| OsmError::pbf(path, e))?));
        }
        if blobs.is_empty() {
            return Ok(());
        }
        let decoded: Vec<Option<T>> = blobs
            .into_par_iter()
            .map(
                |(seq, blob)| match blob.decode().map_err(|e| OsmError::block(path, seq, e))? {
                    BlobDecode::OsmData(block) => decode(&block).map(Some),
                    BlobDecode::OsmHeader(_) | BlobDecode::Unknown(_) => Ok(None),
                },
            )
            .collect::<Result<_, _>>()?;
        for item in decoded.into_iter().flatten() {
            consume(item)?;
        }
    }
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
