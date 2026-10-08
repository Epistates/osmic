//! Applying a [`ChangeSet`] to a sorted PBF file.
//!
//! The input is streamed in file order (blocks decoded in parallel) and
//! merged with the change set, which is already sorted by (type, id): new
//! objects are inserted at their sorted position, modified ones replaced,
//! deleted ones dropped. The output is written to a temporary file in the
//! destination directory and renamed into place only when complete, with
//! the new replication state in its header — so data and state can never
//! disagree, and the input may be updated in place.

use std::io::BufWriter;
use std::path::Path;

use osmpbf::Element as PbfElement;
use tracing::info;

use osmic_core::{FixedCoord, OsmId, OsmType};
use osmic_osm::OsmError;
use osmic_osm::pbf::{PbfWriter, PbfWriterOptions, for_each_block_ordered, read_header};

use crate::changeset::ChangeSet;
use crate::error::ReplError;
use crate::osc::{Element, Member};
use crate::state::ReplicationState;

/// Counts from applying a change set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub read: u64,
    pub written: u64,
    pub created: u64,
    pub modified: u64,
    pub deleted: u64,
}

fn owned<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<(String, String)> {
    tags.map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

fn to_element(e: PbfElement<'_>) -> Element {
    match e {
        PbfElement::Node(n) => Element::Node {
            id: n.id(),
            location: FixedCoord::new(n.decimicro_lon(), n.decimicro_lat()),
            tags: owned(n.tags()),
        },
        PbfElement::DenseNode(n) => Element::Node {
            id: n.id(),
            location: FixedCoord::new(n.decimicro_lon(), n.decimicro_lat()),
            tags: owned(n.tags()),
        },
        PbfElement::Way(w) => Element::Way {
            id: w.id(),
            refs: w.refs().collect(),
            tags: owned(w.tags()),
        },
        PbfElement::Relation(r) => Element::Relation {
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
                    role: m.role().unwrap_or_default().to_owned(),
                })
                .collect(),
            tags: owned(r.tags()),
        },
    }
}

fn borrowed(tags: &[(String, String)]) -> Vec<(&str, &str)> {
    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

fn write<W: std::io::Write>(w: &mut PbfWriter<W>, e: &Element) -> std::io::Result<()> {
    let tags = borrowed;
    match e {
        Element::Node {
            id,
            location,
            tags: t,
        } => w.write_node(*id, *location, &tags(t)),
        Element::Way { id, refs, tags: t } => w.write_way(*id, refs, &tags(t)),
        Element::Relation {
            id,
            members,
            tags: t,
        } => {
            let m: Vec<(OsmType, i64, &str)> = members
                .iter()
                .map(|m| (m.osm_type, m.id, m.role.as_str()))
                .collect();
            w.write_relation(*id, &m, &tags(t))
        }
    }
}

/// Apply `changes` to `input`, writing `output` with `state` recorded in
/// the header. `input` must be sorted by type then id (as produced by
/// `osmium sort` and every major extract provider).
pub fn apply_to_pbf(
    input: &Path,
    output: &Path,
    changes: &ChangeSet,
    state: &ReplicationState,
) -> Result<ApplyStats, ReplError> {
    let header = read_header(input)?;
    if !header.is_sorted() {
        return Err(ReplError::State(format!(
            "{} is not sorted by type and id; sort it first (osmium sort)",
            input.display()
        )));
    }
    let temp = osmic_core::fs::temp_file_for(output)?;
    let options = PbfWriterOptions {
        bbox: header.bbox,
        sorted: true,
        writing_program: Some(concat!("osmic ", env!("CARGO_PKG_VERSION")).into()),
        replication_timestamp: state.unix_timestamp(),
        replication_sequence: i64::try_from(state.sequence).ok(),
        replication_base_url: Some(state.base_url.clone()),
        ..Default::default()
    };
    let mut writer = PbfWriter::new(BufWriter::with_capacity(1 << 20, temp.as_file()), &options)?;
    let mut stats = ApplyStats::default();
    let mut pending = changes.iter().peekable();
    let mut last: Option<OsmId> = None;

    let io = |e: std::io::Error| OsmError::Io(e);
    for_each_block_ordered(
        input,
        64,
        |block| Ok(block.elements().map(to_element).collect::<Vec<_>>()),
        |elements| {
            for e in elements {
                stats.read += 1;
                let id = e.osm_id();
                if last.is_some_and(|l| id <= l) {
                    return Err(OsmError::Pbf {
                        path: input.to_path_buf(),
                        message: format!("element {id} out of order; the file is not sorted"),
                        source: "unsorted input".into(),
                    });
                }
                last = Some(id);
                // New objects that sort before this one.
                while let Some((cid, change)) = pending.next_if(|(cid, _)| **cid < id) {
                    if let Some(new) = change {
                        write(&mut writer, new).map_err(io)?;
                        stats.created += 1;
                        stats.written += 1;
                    }
                    let _ = cid;
                }
                match pending.next_if(|(cid, _)| **cid == id) {
                    Some((_, Some(new))) => {
                        write(&mut writer, new).map_err(io)?;
                        stats.modified += 1;
                        stats.written += 1;
                    }
                    Some((_, None)) => stats.deleted += 1,
                    None => {
                        write(&mut writer, &e).map_err(io)?;
                        stats.written += 1;
                    }
                }
            }
            Ok(())
        },
    )?;
    for (_, change) in pending {
        if let Some(new) = change {
            write(&mut writer, new)?;
            stats.created += 1;
            stats.written += 1;
        }
    }
    writer.finish()?;
    osmic_core::fs::persist(temp, output, true)?;
    info!(
        output = %output.display(),
        sequence = state.sequence,
        created = stats.created,
        modified = stats.modified,
        deleted = stats.deleted,
        "PBF updated"
    );
    Ok(stats)
}
