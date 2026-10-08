#[cfg(feature = "native")]
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Errors from reading OSM data and building features.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OsmError {
    /// Opening or reading an input file failed.
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    /// The PBF container could not be read.
    #[error("{}: {message}", path.display())]
    Pbf {
        /// The PBF file.
        path: PathBuf,
        /// Human-readable cause, with a remedy where one is known.
        message: String,
        /// The underlying `osmpbf` error.
        #[source]
        source: BoxError,
    },

    /// One data block could not be decoded.
    #[error("{}: block {block}: {message}", path.display())]
    Block {
        /// The PBF file.
        path: PathBuf,
        /// Zero-based index of the blob in the file, counting the header.
        block: u64,
        /// Human-readable cause, with a remedy where one is known.
        message: String,
        /// The underlying `osmpbf` error.
        #[source]
        source: BoxError,
    },

    /// The PBF header lists a required feature osmic does not implement,
    /// e.g. `HistoricalInformation` in OSM history files.
    #[error(
        "{}: requires PBF feature '{feature}', which osmic does not support{}",
        path.display(),
        if feature == "HistoricalInformation" {
            " (OSM history files are not supported; use a current snapshot)"
        } else {
            ""
        }
    )]
    UnsupportedFeature {
        /// The PBF file.
        path: PathBuf,
        /// The unsupported feature name, as written in the header.
        feature: String,
    },

    /// The file at this path has no `OSMHeader` block before its first data
    /// block, so it is not a valid PBF file.
    #[error("{}: no OSMHeader block before the first data block", .0.display())]
    MissingHeader(PathBuf),

    /// Creating or writing the node location store failed.
    #[error("node store")]
    NodeStore(#[from] osmic_index::NodeStoreError),

    /// A GeoJSON input could not be parsed; the message names the file.
    #[error("invalid GeoJSON: {0}")]
    GeoJson(String),

    /// A [`crate::pipeline::FeatureSink`] rejected features.
    #[error("feature sink failed")]
    Sink(#[source] BoxError),
}

#[cfg(feature = "native")]
fn describe(e: &osmpbf::Error) -> String {
    match e.kind() {
        osmpbf::ErrorKind::Blob(osmpbf::BlobError::Empty) => "unsupported blob compression \
             (only raw and zlib PBF blobs are supported; re-encode with \
             `osmium cat -f pbf,pbf_compression=zlib`)"
            .to_string(),
        _ => e.to_string(),
    }
}

#[cfg(feature = "native")]
impl OsmError {
    pub(crate) fn pbf(path: &Path, e: osmpbf::Error) -> Self {
        Self::Pbf {
            path: path.to_path_buf(),
            message: describe(&e),
            source: Box::new(e),
        }
    }

    pub(crate) fn block(path: &Path, block: u64, e: osmpbf::Error) -> Self {
        Self::Block {
            path: path.to_path_buf(),
            block,
            message: describe(&e),
            source: Box::new(e),
        }
    }
}
