//! Applying a [`ChangeSet`] to a sorted PBF file.
//!
//! The input is read block by block (decoded in parallel) and merged with
//! the change set, which is sorted in the same (type, id) order: new
//! objects are inserted at their sorted position, modified ones replaced,
//! deleted ones dropped. A block that no change touches is copied verbatim
//! — metadata, string tables and all — so a typical update rewrites only a
//! small fraction of the file and spends no time recompressing the rest.
//! Re-encoded objects keep their metadata (version, timestamp, changeset,
//! user).
//!
//! The output is written to a temporary file in the destination directory
//! and renamed into place only when complete, with the new replication
//! state in its header — so data and state can never disagree, and the
//! input may be updated in place.

use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use osmpbf::{BlobDecode, BlobReader, BlobType, Element as PbfElement, PrimitiveBlock};
use rayon::prelude::*;
use tracing::{info, warn};

use osmic_core::{FixedCoord, OsmId, OsmType};
use osmic_osm::OsmError;
use osmic_osm::pbf::{
    ElementMeta, PbfWriter, PbfWriterOptions, StringTable, location, read_header,
};

use crate::changeset::{ChangeSet, SortKey};
use crate::error::ReplError;
use crate::osc::{Element, Member};
use crate::state::ReplicationState;

/// Blocks decoded concurrently.
const WINDOW: usize = 64;

/// Counts from applying a change set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApplyStats {
    /// Objects read from the input.
    pub read: u64,
    /// Objects written to the output.
    pub written: u64,
    /// Objects written that were not in the input.
    pub created: u64,
    /// Input objects replaced by a newer version.
    pub modified: u64,
    /// Input objects dropped. Deletions of objects the input did not
    /// contain are not counted.
    pub deleted: u64,
    /// Input blocks copied without re-encoding.
    pub blocks_copied: u64,
    /// Input blocks re-encoded because a change fell inside them.
    pub blocks_rewritten: u64,
}

/// One input data block, ready to merge.
enum Block {
    /// No change falls inside the block: copy these framed bytes.
    Untouched {
        raw: Vec<u8>,
        first: OsmId,
        last: OsmId,
        count: u64,
    },
    /// Decoded elements, to merge with the changes.
    Touched(Vec<Element>),
    /// An empty data block: nothing to write.
    Skip,
}

fn pbf_error(path: &Path, message: String) -> OsmError {
    OsmError::Pbf {
        path: path.to_path_buf(),
        message,
        source: "invalid input".into(),
    }
}

fn pbf_failure(path: &Path, e: osmpbf::Error) -> OsmError {
    OsmError::Pbf {
        path: path.to_path_buf(),
        message: e.to_string(),
        source: Box::new(e),
    }
}

fn out_of_order(path: &Path, id: OsmId) -> ReplError {
    pbf_error(
        path,
        format!("element {id} out of order; the file is not sorted"),
    )
    .into()
}

/// Byte range of every blob in the file (each starts with its 4-byte
/// length prefix), and whether it is a data blob.
fn blob_ranges(input: &Path) -> Result<Vec<(Range<u64>, bool)>, ReplError> {
    let mut reader = BlobReader::seekable_from_path(input).map_err(|e| pbf_failure(input, e))?;
    let mut starts = Vec::new();
    while let Some(next) = reader.next_header_skip_blob() {
        let (header, offset) = next.map_err(|e| pbf_failure(input, e))?;
        let offset = offset.ok_or_else(|| pbf_error(input, "blob without an offset".into()))?;
        starts.push((offset.0, matches!(header.blob_type(), BlobType::OsmData)));
    }
    let len = std::fs::metadata(input)?.len();
    Ok(starts
        .iter()
        .enumerate()
        .map(|(i, &(start, data))| {
            let end = starts.get(i + 1).map_or(len, |&(next, _)| next);
            (start..end, data)
        })
        .collect())
}

fn meta_of(
    version: Option<i32>,
    milli_timestamp: Option<i64>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user: &str,
) -> Option<ElementMeta> {
    Some(ElementMeta {
        version: version?,
        timestamp: milli_timestamp.unwrap_or(0).div_euclid(1000),
        changeset: changeset.unwrap_or(0),
        uid: uid.unwrap_or(0),
        user: user.to_owned(),
    })
}

fn owned<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<(String, String)> {
    tags.map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

fn node_location(
    nano_lon: i64,
    nano_lat: i64,
    id: i64,
    input: &Path,
) -> Result<FixedCoord, ReplError> {
    location(nano_lon, nano_lat)
        .ok_or_else(|| pbf_error(input, format!("node {id} has an invalid location")).into())
}

/// Decode a block's elements, metadata included, with lossless tags.
fn decode_elements(block: &PrimitiveBlock, input: &Path) -> Result<Vec<Element>, ReplError> {
    let strings = StringTable::new(block);
    let mut out = Vec::new();
    for e in block.elements() {
        out.push(match e {
            PbfElement::Node(n) => {
                let info = n.info();
                let user = info.user().and_then(Result::ok).unwrap_or_default();
                Element::Node {
                    id: n.id(),
                    location: node_location(n.nano_lon(), n.nano_lat(), n.id(), input)?,
                    tags: owned(strings.tags(n.raw_tags())),
                    meta: meta_of(
                        info.version(),
                        info.milli_timestamp(),
                        info.changeset(),
                        info.uid(),
                        user,
                    ),
                }
            }
            PbfElement::DenseNode(n) => {
                let meta = n.info().and_then(|i| {
                    meta_of(
                        Some(i.version()),
                        Some(i.milli_timestamp()),
                        Some(i.changeset()),
                        Some(i.uid()),
                        i.user().unwrap_or_default(),
                    )
                });
                Element::Node {
                    id: n.id(),
                    location: node_location(n.nano_lon(), n.nano_lat(), n.id(), input)?,
                    tags: owned(strings.tags(n.raw_tags())),
                    meta,
                }
            }
            PbfElement::Way(w) => {
                let info = w.info();
                let user = info.user().and_then(Result::ok).unwrap_or_default();
                Element::Way {
                    id: w.id(),
                    refs: w.refs().collect(),
                    tags: owned(strings.tags(w.raw_tags())),
                    meta: meta_of(
                        info.version(),
                        info.milli_timestamp(),
                        info.changeset(),
                        info.uid(),
                        user,
                    ),
                }
            }
            PbfElement::Relation(r) => {
                let info = r.info();
                let user = info.user().and_then(Result::ok).unwrap_or_default();
                Element::Relation {
                    id: r.id(),
                    members: r
                        .members()
                        .map(|m| Member {
                            osm_type: match m.member_type {
                                osmpbf::RelMemberType::Node => OsmType::Node,
                                osmpbf::RelMemberType::Way => OsmType::Way,
                                osmpbf::RelMemberType::Relation => OsmType::Relation,
                            },
                            id: m.member_id,
                            role: usize::try_from(m.role_sid)
                                .ok()
                                .and_then(|i| strings.get(i))
                                .unwrap_or_default()
                                .to_owned(),
                        })
                        .collect(),
                    tags: owned(strings.tags(r.raw_tags())),
                    meta: meta_of(
                        info.version(),
                        info.milli_timestamp(),
                        info.changeset(),
                        info.uid(),
                        user,
                    ),
                }
            }
        });
    }
    Ok(out)
}

/// Read blob `range`, decide whether any change touches it and decode it
/// only if one does.
fn read_block(input: &Path, range: &Range<u64>, changes: &ChangeSet) -> Result<Block, ReplError> {
    let len = usize::try_from(range.end - range.start).map_err(std::io::Error::other)?;
    let mut raw = vec![0u8; len];
    let mut file = File::open(input)?;
    file.seek(SeekFrom::Start(range.start))?;
    file.read_exact(&mut raw)?;
    let blob = BlobReader::new(Cursor::new(&raw[..]))
        .next()
        .ok_or_else(|| pbf_error(input, format!("no blob at byte {}", range.start)))?
        .map_err(|e| pbf_failure(input, e))?;
    let BlobDecode::OsmData(block) = blob.decode().map_err(|e| pbf_failure(input, e))? else {
        return Ok(Block::Skip);
    };
    // Ids in file order, checked to be strictly increasing.
    let (mut first, mut last): (Option<OsmId>, Option<OsmId>) = (None, None);
    let mut count = 0u64;
    for e in block.elements() {
        let id = match e {
            PbfElement::Node(n) => OsmId::node(n.id()),
            PbfElement::DenseNode(n) => OsmId::node(n.id()),
            PbfElement::Way(w) => OsmId::way(w.id()),
            PbfElement::Relation(r) => OsmId::relation(r.id()),
        };
        if last.is_some_and(|l| SortKey(id) <= SortKey(l)) {
            return Err(out_of_order(input, id));
        }
        first.get_or_insert(id);
        last = Some(id);
        count += 1;
    }
    let (Some(first), Some(last)) = (first, last) else {
        return Ok(Block::Skip);
    };
    if changes.touches(first, last) {
        return Ok(Block::Touched(decode_elements(&block, input)?));
    }
    drop(block);
    drop(blob);
    Ok(Block::Untouched {
        raw,
        first,
        last,
        count,
    })
}

fn borrowed(tags: &[(String, String)]) -> Vec<(&str, &str)> {
    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

fn write<W: std::io::Write>(w: &mut PbfWriter<W>, e: &Element) -> std::io::Result<()> {
    match e {
        Element::Node {
            id,
            location,
            tags,
            meta,
        } => w.write_node_with_meta(*id, *location, &borrowed(tags), meta.as_ref()),
        Element::Way {
            id,
            refs,
            tags,
            meta,
        } => w.write_way_with_meta(*id, refs, &borrowed(tags), meta.as_ref()),
        Element::Relation {
            id,
            members,
            tags,
            meta,
        } => {
            let m: Vec<(OsmType, i64, &str)> = members
                .iter()
                .map(|m| (m.osm_type, m.id, m.role.as_str()))
                .collect();
            w.write_relation_with_meta(*id, &m, &borrowed(tags), meta.as_ref())
        }
    }
}

type Pending<'c> =
    std::iter::Peekable<Box<dyn Iterator<Item = (&'c OsmId, &'c Option<Element>)> + 'c>>;

/// Merge state: pending changes and the last object written.
struct Merge<'c, W: std::io::Write> {
    writer: PbfWriter<W>,
    pending: Pending<'c>,
    last: Option<OsmId>,
    stats: ApplyStats,
}

impl<W: std::io::Write> Merge<'_, W> {
    fn check_order(&self, id: OsmId, input: &Path) -> Result<(), ReplError> {
        if self.last.is_some_and(|l| SortKey(id) <= SortKey(l)) {
            return Err(out_of_order(input, id));
        }
        Ok(())
    }

    /// Write every pending creation that sorts before `id`.
    fn creations_before(&mut self, id: OsmId) -> Result<(), ReplError> {
        while let Some((_, change)) = self
            .pending
            .next_if(|(cid, _)| SortKey(**cid) < SortKey(id))
        {
            if let Some(new) = change {
                write(&mut self.writer, new)?;
                self.stats.created += 1;
                self.stats.written += 1;
            }
        }
        Ok(())
    }

    fn element(&mut self, e: &Element, input: &Path) -> Result<(), ReplError> {
        let id = e.osm_id();
        self.check_order(id, input)?;
        self.last = Some(id);
        self.stats.read += 1;
        self.creations_before(id)?;
        match self.pending.next_if(|(cid, _)| **cid == id) {
            Some((_, Some(new))) => {
                write(&mut self.writer, new)?;
                self.stats.modified += 1;
                self.stats.written += 1;
            }
            Some((_, None)) => self.stats.deleted += 1,
            None => {
                write(&mut self.writer, e)?;
                self.stats.written += 1;
            }
        }
        Ok(())
    }
}

/// Apply `changes` to `input`, writing `output` with `state` recorded in
/// the header. `input` must be sorted by type then id (as produced by
/// `osmium sort` and every major extract provider).
///
/// Blobs of unknown type in the input are dropped, with a warning.
///
/// # Errors
///
/// - [`ReplError::UnsupportedInput`] if the header does not declare the
///   input sorted, or declares node locations on ways.
/// - [`ReplError::Osm`] if the input cannot be read or decoded, or turns
///   out not to be sorted after all.
/// - [`ReplError::Io`] if reading the input or writing the output fails.
///   An existing output is only ever replaced by a complete file.
pub fn apply_to_pbf(
    input: &Path,
    output: &Path,
    changes: &ChangeSet,
    state: &ReplicationState,
) -> Result<ApplyStats, ReplError> {
    let header = read_header(input)?;
    if !header.is_sorted() {
        return Err(ReplError::UnsupportedInput {
            path: input.to_path_buf(),
            reason: "it is not sorted by type and id; sort it first (osmium sort)",
        });
    }
    if header.has_locations_on_ways() {
        return Err(ReplError::UnsupportedInput {
            path: input.to_path_buf(),
            reason: "it stores node locations on ways, which changed ways cannot be given \
                     here; update a file without them and add them afterwards \
                     (osmium add-locations-to-ways)",
        });
    }
    let ranges = blob_ranges(input)?;
    let temp = osmic_core::fs::temp_file_for(output)?;
    let options = PbfWriterOptions::new()
        .bbox(header.bbox)
        .sorted(true)
        .source(header.source.clone())
        .writing_program(concat!("osmic ", env!("CARGO_PKG_VERSION")))
        .replication(
            state.unix_timestamp(),
            i64::try_from(state.sequence).ok(),
            Some(state.base_url.clone()),
        );
    let writer = PbfWriter::new(BufWriter::with_capacity(1 << 20, temp.reopen()?), &options)?;
    let iter: Box<dyn Iterator<Item = (&OsmId, &Option<Element>)>> = Box::new(changes.iter());
    let mut merge = Merge {
        writer,
        pending: iter.peekable(),
        last: None,
        stats: ApplyStats::default(),
    };

    let data: Vec<&Range<u64>> = ranges.iter().filter(|(_, d)| *d).map(|(r, _)| r).collect();
    if ranges.len() > data.len() + 1 {
        warn!(
            skipped = ranges.len() - data.len() - 1,
            "input has blobs of unknown type; they are not copied"
        );
    }
    for window in data.chunks(WINDOW) {
        let blocks = window
            .par_iter()
            .map(|range| read_block(input, range, changes))
            .collect::<Result<Vec<_>, _>>()?;
        for block in blocks {
            match block {
                Block::Skip => {}
                Block::Untouched {
                    raw,
                    first,
                    last,
                    count,
                } => {
                    merge.check_order(first, input)?;
                    merge.creations_before(first)?;
                    merge.writer.write_raw_blob(&raw)?;
                    merge.last = Some(last);
                    merge.stats.read += count;
                    merge.stats.written += count;
                    merge.stats.blocks_copied += 1;
                }
                Block::Touched(elements) => {
                    for e in &elements {
                        merge.element(e, input)?;
                    }
                    merge.stats.blocks_rewritten += 1;
                }
            }
        }
    }
    for (_, change) in merge.pending.by_ref() {
        if let Some(new) = change {
            write(&mut merge.writer, new)?;
            merge.stats.created += 1;
            merge.stats.written += 1;
        }
    }
    let Merge { writer, stats, .. } = merge;
    drop(writer.finish()?);
    osmic_core::fs::persist(temp, output, true)?;
    info!(
        output = %output.display(),
        sequence = state.sequence,
        created = stats.created,
        modified = stats.modified,
        deleted = stats.deleted,
        blocks_copied = stats.blocks_copied,
        blocks_rewritten = stats.blocks_rewritten,
        "PBF updated"
    );
    Ok(stats)
}
