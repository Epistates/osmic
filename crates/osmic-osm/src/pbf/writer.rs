//! Spec-conformant OSM PBF writer.
//!
//! Writes an `OSMHeader` block followed by zlib-compressed `OSMData` blocks.
//! Each block holds one element type (dense nodes, ways or relations), at
//! most 8 000 entities and at most [`MAX_BLOCK_BYTES`] of uncompressed data,
//! well inside the 32 MiB that readers (osmpbf, libosmium) accept.
//! Coordinates use the default granularity (100 nanodegrees = 1e-7°), so
//! [`FixedCoord`] values are written exactly. Elements must be written
//! nodes, then ways, then relations for the file to be flagged
//! `Sort.Type_then_ID` (when also sorted by id, see
//! [`PbfWriterOptions::sorted`]).
//!
//! Metadata (version, timestamp, changeset, user) is written for elements
//! that carry an [`ElementMeta`]. Way node locations are written when
//! [`PbfWriterOptions::locations_on_ways`] is set. Already-encoded blocks
//! from another file can be copied verbatim with
//! [`PbfWriter::write_raw_blob`]. Blocks are compressed in parallel batches.

use std::collections::HashMap;
use std::io::{self, Write};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use rayon::prelude::*;

use osmic_core::{BBox, FixedCoord, OsmType};

const MAX_ENTITIES_PER_BLOCK: usize = 8_000;
/// Uncompressed size at which a block is flushed.
pub const MAX_BLOCK_BYTES: usize = 16 << 20;
/// Largest single element accepted (its block must stay under 32 MiB).
const MAX_ELEMENT_BYTES: usize = 24 << 20;
/// Blocks encoded before a parallel compression batch is written.
const PENDING_BLOCKS: usize = 64;

/// Header options for [`PbfWriter`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PbfWriterOptions {
    /// Bounding box written to the header.
    pub bbox: Option<BBox>,
    /// Declare `Sort.Type_then_ID`. Only set this if elements are written
    /// in type-then-id order.
    pub sorted: bool,
    /// `writingprogram` header field (defaults to `osmic`).
    pub writing_program: Option<String>,
    /// `source` header field.
    pub source: Option<String>,
    /// Write way node locations (`LocationsOnWays`); ways must then be
    /// written with [`PbfWriter::write_way_with_locations`].
    pub locations_on_ways: bool,
    /// Additional `required_features` to declare (e.g. to produce test
    /// files that readers must reject).
    pub extra_required_features: Vec<String>,
    /// Replication state (`osmosis_replication_*` header fields): the
    /// timestamp (Unix seconds), sequence number and base URL the data is
    /// current to.
    pub replication_timestamp: Option<i64>,
    /// Replication sequence number (see `replication_timestamp`).
    pub replication_sequence: Option<i64>,
    /// Replication base URL (see `replication_timestamp`).
    pub replication_base_url: Option<String>,
}

impl PbfWriterOptions {
    /// Default options: unsorted, no bbox, no replication state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `Sort.Type_then_ID`.
    #[must_use]
    pub fn sorted(mut self, sorted: bool) -> Self {
        self.sorted = sorted;
        self
    }

    /// Bounding box written to the header.
    #[must_use]
    pub fn bbox(mut self, bbox: Option<BBox>) -> Self {
        self.bbox = bbox;
        self
    }

    /// `writingprogram` header field.
    #[must_use]
    pub fn writing_program(mut self, program: impl Into<String>) -> Self {
        self.writing_program = Some(program.into());
        self
    }

    /// `source` header field.
    #[must_use]
    pub fn source(mut self, source: Option<String>) -> Self {
        self.source = source;
        self
    }

    /// Write way node locations (`LocationsOnWays`).
    #[must_use]
    pub fn locations_on_ways(mut self, on: bool) -> Self {
        self.locations_on_ways = on;
        self
    }

    /// Declare an additional required feature.
    #[must_use]
    pub fn required_feature(mut self, feature: impl Into<String>) -> Self {
        self.extra_required_features.push(feature.into());
        self
    }

    /// Replication state: timestamp (Unix seconds), sequence and base URL.
    #[must_use]
    pub fn replication(
        mut self,
        timestamp: Option<i64>,
        sequence: Option<i64>,
        base_url: Option<String>,
    ) -> Self {
        self.replication_timestamp = timestamp;
        self.replication_sequence = sequence;
        self.replication_base_url = base_url;
        self
    }
}

/// Object metadata as stored in PBF `Info` / `DenseInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElementMeta {
    /// Object version, starting at 1.
    pub version: i32,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    /// Id of the changeset that wrote this version.
    pub changeset: i64,
    /// Id of the user who wrote this version.
    pub uid: i32,
    /// Display name of that user.
    pub user: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Nodes,
    Ways,
    Relations,
}

/// Streaming PBF writer.
pub struct PbfWriter<W: Write> {
    out: W,
    locations_on_ways: bool,
    group: Option<Group>,
    strings: StringTable,
    count: usize,
    /// Upper bound of the current block's encoded size.
    bytes: usize,
    // Dense nodes.
    ids: Vec<i64>,
    lats: Vec<i64>,
    lons: Vec<i64>,
    keys_vals: Vec<u32>,
    /// Per-node metadata for the current dense block (`None` until a node
    /// with metadata is written; then one entry per node).
    dense_meta: Option<Vec<DenseMeta>>,
    // Ways / relations, already encoded.
    elements: Vec<u8>,
    // Encoded, not yet compressed blocks.
    pending: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct DenseMeta {
    version: i32,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user_sid: u32,
}

fn too_large(what: &str, bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{what} of {bytes} bytes exceeds the {MAX_ELEMENT_BYTES}-byte PBF element limit"),
    )
}

impl<W: Write> PbfWriter<W> {
    /// Start a file: writes the header block immediately.
    ///
    /// # Errors
    ///
    /// Any error from writing the header to `out`.
    pub fn new(mut out: W, options: &PbfWriterOptions) -> io::Result<Self> {
        let mut header = Vec::new();
        if let Some(b) = options.bbox {
            let mut bbox = Vec::new();
            let nano = |deg: f64| (deg * 1e9).round() as i64;
            put_sint64(&mut bbox, 1, nano(b.min_lon));
            put_sint64(&mut bbox, 2, nano(b.max_lon));
            put_sint64(&mut bbox, 3, nano(b.max_lat));
            put_sint64(&mut bbox, 4, nano(b.min_lat));
            put_bytes(&mut header, 1, &bbox);
        }
        put_bytes(&mut header, 4, b"OsmSchema-V0.6");
        put_bytes(&mut header, 4, b"DenseNodes");
        for f in &options.extra_required_features {
            put_bytes(&mut header, 4, f.as_bytes());
        }
        if options.sorted {
            put_bytes(&mut header, 5, b"Sort.Type_then_ID");
        }
        if options.locations_on_ways {
            put_bytes(&mut header, 5, b"LocationsOnWays");
        }
        let program = options.writing_program.as_deref().unwrap_or("osmic");
        put_bytes(&mut header, 16, program.as_bytes());
        if let Some(source) = &options.source {
            put_bytes(&mut header, 17, source.as_bytes());
        }
        if let Some(t) = options.replication_timestamp {
            put_int64(&mut header, 32, t);
        }
        if let Some(seq) = options.replication_sequence {
            put_int64(&mut header, 33, seq);
        }
        if let Some(url) = &options.replication_base_url {
            put_bytes(&mut header, 34, url.as_bytes());
        }
        out.write_all(&frame_blob("OSMHeader", &header)?)?;
        Ok(Self {
            out,
            locations_on_ways: options.locations_on_ways,
            group: None,
            strings: StringTable::default(),
            count: 0,
            bytes: 0,
            ids: Vec::new(),
            lats: Vec::new(),
            lons: Vec::new(),
            keys_vals: Vec::new(),
            dense_meta: None,
            elements: Vec::new(),
            pending: Vec::new(),
        })
    }

    /// Start an element of `group` whose encoding takes at most `size`
    /// bytes (strings included), flushing the current block first if the
    /// element would not fit.
    fn begin(&mut self, group: Group, size: usize) -> io::Result<()> {
        if size > MAX_ELEMENT_BYTES {
            return Err(too_large("element", size));
        }
        if self.group != Some(group)
            || self.count >= MAX_ENTITIES_PER_BLOCK
            || self.bytes + size > MAX_BLOCK_BYTES
        {
            self.flush_block()?;
            self.group = Some(group);
        }
        self.count += 1;
        self.bytes += size;
        Ok(())
    }

    /// Write a node.
    ///
    /// # Errors
    ///
    /// As [`Self::write_node_with_meta`].
    pub fn write_node(
        &mut self,
        id: i64,
        coord: FixedCoord,
        tags: &[(&str, &str)],
    ) -> io::Result<()> {
        self.write_node_with_meta(id, coord, tags, None)
    }

    /// Write a node with optional metadata.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the encoded node would exceed the
    /// 24 MiB element limit; any error from writing completed blocks to the
    /// output, which happens in batches.
    pub fn write_node_with_meta(
        &mut self,
        id: i64,
        coord: FixedCoord,
        tags: &[(&str, &str)],
        meta: Option<&ElementMeta>,
    ) -> io::Result<()> {
        let size = 64 + tags_size(tags) + meta_size(meta);
        self.begin(Group::Nodes, size)?;
        if meta.is_some() && self.dense_meta.is_none() {
            // Earlier nodes of this block have no metadata.
            self.dense_meta = Some(vec![DenseMeta::default(); self.ids.len()]);
        }
        if let Some(all) = self.dense_meta.as_mut() {
            all.push(match meta {
                Some(m) => DenseMeta {
                    version: m.version,
                    timestamp: m.timestamp,
                    changeset: m.changeset,
                    uid: m.uid,
                    user_sid: self.strings.index(&m.user),
                },
                None => DenseMeta::default(),
            });
        }
        self.ids.push(id);
        self.lats.push(i64::from(coord.lat));
        self.lons.push(i64::from(coord.lon));
        for (k, v) in tags {
            self.keys_vals.push(self.strings.index(k));
            self.keys_vals.push(self.strings.index(v));
        }
        self.keys_vals.push(0);
        Ok(())
    }

    /// Write a way.
    ///
    /// # Errors
    ///
    /// As [`Self::write_way_with_meta`].
    pub fn write_way(&mut self, id: i64, refs: &[i64], tags: &[(&str, &str)]) -> io::Result<()> {
        self.write_way_with_meta(id, refs, tags, None)
    }

    /// Write a way with optional metadata.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the encoded way would exceed the
    /// 24 MiB element limit; any error from writing completed blocks to the
    /// output, which happens in batches.
    pub fn write_way_with_meta(
        &mut self,
        id: i64,
        refs: &[i64],
        tags: &[(&str, &str)],
        meta: Option<&ElementMeta>,
    ) -> io::Result<()> {
        self.way(id, refs, None, tags, meta)
    }

    /// Write a way with the location of each node (`None` for a missing
    /// node, written as an out-of-range location like osmium does). Needs
    /// [`PbfWriterOptions::locations_on_ways`].
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] without `locations_on_ways`, if
    /// `locations` and `refs` differ in length, or past the element limit;
    /// otherwise as [`Self::write_way_with_meta`].
    pub fn write_way_with_locations(
        &mut self,
        id: i64,
        refs: &[i64],
        locations: &[Option<FixedCoord>],
        tags: &[(&str, &str)],
        meta: Option<&ElementMeta>,
    ) -> io::Result<()> {
        if !self.locations_on_ways || locations.len() != refs.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "way locations need locations_on_ways and one location per ref",
            ));
        }
        self.way(id, refs, Some(locations), tags, meta)
    }

    fn way(
        &mut self,
        id: i64,
        refs: &[i64],
        locations: Option<&[Option<FixedCoord>]>,
        tags: &[(&str, &str)],
        meta: Option<&ElementMeta>,
    ) -> io::Result<()> {
        let size = 32 + refs.len() * 30 + tags_size(tags) + meta_size(meta);
        self.begin(Group::Ways, size)?;
        let mut way = Vec::new();
        put_int64(&mut way, 1, id);
        self.put_tags(&mut way, tags);
        self.put_info(&mut way, meta);
        put_packed_delta_sint64(&mut way, 8, refs);
        if let Some(locations) = locations {
            let (lats, lons): (Vec<i64>, Vec<i64>) = locations
                .iter()
                .map(|l| match l {
                    Some(c) => (i64::from(c.lat), i64::from(c.lon)),
                    None => (i64::from(i32::MAX), i64::from(i32::MAX)),
                })
                .unzip();
            put_packed_delta_sint64(&mut way, 9, &lats);
            put_packed_delta_sint64(&mut way, 10, &lons);
        }
        put_bytes(&mut self.elements, 3, &way);
        Ok(())
    }

    /// Write a relation; members are `(type, id, role)`.
    ///
    /// # Errors
    ///
    /// As [`Self::write_relation_with_meta`].
    pub fn write_relation(
        &mut self,
        id: i64,
        members: &[(OsmType, i64, &str)],
        tags: &[(&str, &str)],
    ) -> io::Result<()> {
        self.write_relation_with_meta(id, members, tags, None)
    }

    /// Write a relation with optional metadata.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the encoded relation would exceed
    /// the 24 MiB element limit; any error from writing completed blocks to
    /// the output, which happens in batches.
    pub fn write_relation_with_meta(
        &mut self,
        id: i64,
        members: &[(OsmType, i64, &str)],
        tags: &[(&str, &str)],
        meta: Option<&ElementMeta>,
    ) -> io::Result<()> {
        let size = 32
            + members.iter().map(|m| 25 + m.2.len()).sum::<usize>()
            + tags_size(tags)
            + meta_size(meta);
        self.begin(Group::Relations, size)?;
        let mut rel = Vec::new();
        put_int64(&mut rel, 1, id);
        self.put_tags(&mut rel, tags);
        self.put_info(&mut rel, meta);
        let roles: Vec<u32> = members.iter().map(|m| self.strings.index(m.2)).collect();
        put_packed_varint(&mut rel, 8, roles.iter().map(|&r| u64::from(r)));
        let ids: Vec<i64> = members.iter().map(|m| m.1).collect();
        put_packed_delta_sint64(&mut rel, 9, &ids);
        put_packed_varint(
            &mut rel,
            10,
            members.iter().map(|m| match m.0 {
                OsmType::Node => 0,
                OsmType::Way => 1,
                OsmType::Relation => 2,
            }),
        );
        put_bytes(&mut self.elements, 4, &rel);
        Ok(())
    }

    /// Copy an already-framed blob (length prefix, `BlobHeader`, `Blob`)
    /// from another PBF file, after everything written so far. The blob is
    /// not validated.
    ///
    /// # Errors
    ///
    /// Any error from writing to the output.
    pub fn write_raw_blob(&mut self, framed: &[u8]) -> io::Result<()> {
        self.flush_block()?;
        self.write_pending()?;
        self.out.write_all(framed)
    }

    fn put_tags(&mut self, buf: &mut Vec<u8>, tags: &[(&str, &str)]) {
        let keys: Vec<u64> = tags
            .iter()
            .map(|t| u64::from(self.strings.index(t.0)))
            .collect();
        let vals: Vec<u64> = tags
            .iter()
            .map(|t| u64::from(self.strings.index(t.1)))
            .collect();
        put_packed_varint(buf, 2, keys);
        put_packed_varint(buf, 3, vals);
    }

    /// `Info` message (field 4 of ways and relations).
    fn put_info(&mut self, buf: &mut Vec<u8>, meta: Option<&ElementMeta>) {
        let Some(m) = meta else { return };
        let mut info = Vec::new();
        put_int64(&mut info, 1, i64::from(m.version));
        put_int64(&mut info, 2, m.timestamp);
        put_int64(&mut info, 3, m.changeset);
        put_int64(&mut info, 4, i64::from(m.uid));
        put_varint_field(&mut info, 5, u64::from(self.strings.index(&m.user)));
        put_bytes(buf, 4, &info);
    }

    fn flush_block(&mut self) -> io::Result<()> {
        let Some(group) = self.group.take() else {
            return Ok(());
        };
        let mut group_buf = Vec::new();
        match group {
            Group::Nodes => {
                let mut dense = Vec::new();
                put_packed_delta_sint64(&mut dense, 1, &self.ids);
                if let Some(meta) = self.dense_meta.take() {
                    let mut info = Vec::new();
                    put_packed_varint(
                        &mut info,
                        1,
                        meta.iter().map(|m| u64::from(m.version as u32)),
                    );
                    let delta = |f: &dyn Fn(&DenseMeta) -> i64| -> Vec<i64> {
                        meta.iter().map(f).collect()
                    };
                    put_packed_delta_sint64(&mut info, 2, &delta(&|m| m.timestamp));
                    put_packed_delta_sint64(&mut info, 3, &delta(&|m| m.changeset));
                    put_packed_delta_sint64(&mut info, 4, &delta(&|m| i64::from(m.uid)));
                    put_packed_delta_sint64(&mut info, 5, &delta(&|m| i64::from(m.user_sid)));
                    put_bytes(&mut dense, 5, &info);
                }
                put_packed_delta_sint64(&mut dense, 8, &self.lats);
                put_packed_delta_sint64(&mut dense, 9, &self.lons);
                put_packed_varint(&mut dense, 10, self.keys_vals.iter().map(|&k| u64::from(k)));
                put_bytes(&mut group_buf, 2, &dense);
            }
            Group::Ways | Group::Relations => group_buf.append(&mut self.elements),
        }
        let mut block = Vec::new();
        let mut table = Vec::new();
        for s in self.strings.strings.drain(..) {
            put_bytes(&mut table, 1, s.as_bytes());
        }
        put_bytes(&mut block, 1, &table);
        put_bytes(&mut block, 2, &group_buf);
        self.pending.push(block);
        if self.pending.len() >= PENDING_BLOCKS {
            self.write_pending()?;
        }

        self.strings = StringTable::default();
        self.count = 0;
        self.bytes = 0;
        self.ids.clear();
        self.lats.clear();
        self.lons.clear();
        self.keys_vals.clear();
        self.dense_meta = None;
        self.elements.clear();
        Ok(())
    }

    /// Compress pending blocks in parallel and write them in order.
    fn write_pending(&mut self) -> io::Result<()> {
        let framed: Vec<Vec<u8>> = self
            .pending
            .par_iter()
            .map(|block| frame_blob("OSMData", block))
            .collect::<io::Result<_>>()?;
        for f in framed {
            self.out.write_all(&f)?;
        }
        self.pending.clear();
        Ok(())
    }

    /// Flush the last block and return the underlying writer.
    ///
    /// # Errors
    ///
    /// Any error from writing or flushing the output.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_block()?;
        self.write_pending()?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Upper bound of the encoded size of `tags` (string table + indices).
fn tags_size(tags: &[(&str, &str)]) -> usize {
    tags.iter().map(|(k, v)| k.len() + v.len() + 20).sum()
}

fn meta_size(meta: Option<&ElementMeta>) -> usize {
    meta.map_or(0, |m| 64 + m.user.len())
}

/// Per-block string table. Index 0 is the mandatory empty string, which
/// dense nodes also use as their key/value delimiter, so an empty string
/// that is actually used gets its own index.
struct StringTable {
    strings: Vec<String>,
    index: HashMap<String, u32>,
}

impl Default for StringTable {
    fn default() -> Self {
        Self {
            strings: vec![String::new()],
            index: HashMap::new(),
        }
    }
}

impl StringTable {
    fn index(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        let i = self.strings.len() as u32;
        self.strings.push(s.to_string());
        self.index.insert(s.to_string(), i);
        i
    }
}

/// Compress `payload` and frame it as a blob (length, header, blob).
fn frame_blob(kind: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(payload)?;
    let compressed = z.finish()?;
    let mut blob = Vec::new();
    put_varint_field(&mut blob, 2, payload.len() as u64);
    put_bytes(&mut blob, 3, &compressed);
    let mut header = Vec::new();
    put_bytes(&mut header, 1, kind.as_bytes());
    put_varint_field(&mut header, 3, blob.len() as u64);
    let header_len = u32::try_from(header.len()).map_err(io::Error::other)?;
    let mut out = Vec::with_capacity(4 + header.len() + blob.len());
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&blob);
    Ok(out)
}

// ── Protobuf encoding ───────────────────────────────────────────────────────

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn put_key(buf: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(buf, (u64::from(field) << 3) | u64::from(wire));
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn put_varint_field(buf: &mut Vec<u8>, field: u32, v: u64) {
    put_key(buf, field, 0);
    put_varint(buf, v);
}

fn put_int64(buf: &mut Vec<u8>, field: u32, v: i64) {
    put_varint_field(buf, field, v as u64);
}

fn put_sint64(buf: &mut Vec<u8>, field: u32, v: i64) {
    put_varint_field(buf, field, zigzag(v));
}

fn put_bytes(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_key(buf, field, 2);
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

fn put_packed_varint(buf: &mut Vec<u8>, field: u32, values: impl IntoIterator<Item = u64>) {
    let mut packed = Vec::new();
    for v in values {
        put_varint(&mut packed, v);
    }
    if !packed.is_empty() {
        put_bytes(buf, field, &packed);
    }
}

fn put_packed_delta_sint64(buf: &mut Vec<u8>, field: u32, values: &[i64]) {
    let mut prev = 0i64;
    put_packed_varint(
        buf,
        field,
        values.iter().map(|&v| {
            let d = v.wrapping_sub(prev);
            prev = v;
            zigzag(d)
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use osmpbf::{Element, ElementReader, RelMemberType};

    #[test]
    fn written_file_reads_back_exactly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.osm.pbf");
        let opts = PbfWriterOptions::new().sorted(true).replication(
            Some(1_791_460_800),
            Some(4_242),
            Some("https://example.org/r/".into()),
        );
        let mut w =
            PbfWriter::new(std::fs::File::create(&path).expect("create"), &opts).expect("header");
        // Enough nodes to span two blocks, including negative coordinates.
        for i in 0..9_000i64 {
            let tags: &[(&str, &str)] = if i == 5 {
                &[("amenity", "cafe"), ("name", "Café")]
            } else {
                &[]
            };
            w.write_node(
                i + 1,
                FixedCoord::new(-1_224_194_155 + i as i32, 377_749_295 - i as i32),
                tags,
            )
            .expect("node");
        }
        w.write_way(100, &[1, 2, 3, 1], &[("building", "yes")])
            .expect("way");
        w.write_relation(
            200,
            &[(OsmType::Way, 100, "outer"), (OsmType::Node, 5, "label")],
            &[("type", "multipolygon"), ("landuse", "grass")],
        )
        .expect("relation");
        w.finish().expect("finish");

        let header = crate::pbf::read_header(&path).expect("header");
        assert!(header.is_sorted());
        assert_eq!(header.replication_sequence, Some(4_242));
        assert_eq!(header.replication_timestamp, Some(1_791_460_800));
        assert_eq!(
            header.replication_base_url.as_deref(),
            Some("https://example.org/r/")
        );
        assert_eq!(header.writing_program.as_deref(), Some("osmic"));

        let mut nodes = 0;
        ElementReader::from_path(&path)
            .expect("open")
            .for_each(|e| match e {
                Element::DenseNode(n) => {
                    let i = n.id() - 1;
                    assert_eq!(n.decimicro_lon(), -1_224_194_155 + i as i32);
                    assert_eq!(n.decimicro_lat(), 377_749_295 - i as i32);
                    if n.id() == 6 {
                        let tags: Vec<_> = n.tags().collect();
                        assert_eq!(tags, [("amenity", "cafe"), ("name", "Café")]);
                    } else {
                        assert_eq!(n.tags().count(), 0);
                    }
                    nodes += 1;
                }
                Element::Way(way) => {
                    assert_eq!(way.id(), 100);
                    assert_eq!(way.refs().collect::<Vec<_>>(), [1, 2, 3, 1]);
                    assert_eq!(way.tags().collect::<Vec<_>>(), [("building", "yes")]);
                }
                Element::Relation(r) => {
                    assert_eq!(r.id(), 200);
                    let m: Vec<_> = r
                        .members()
                        .map(|m| {
                            let role = m.role().expect("utf8").to_string();
                            (m.member_type, m.member_id, role)
                        })
                        .collect();
                    assert_eq!(
                        m,
                        [
                            (RelMemberType::Way, 100, "outer".to_string()),
                            (RelMemberType::Node, 5, "label".to_string())
                        ]
                    );
                }
                Element::Node(_) => panic!("writer emits dense nodes only"),
            })
            .expect("read");
        assert_eq!(nodes, 9_000);
    }

    fn temp_pbf() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.osm.pbf");
        (dir, path)
    }

    #[test]
    fn metadata_round_trips_for_every_element_type() {
        let (_dir, path) = temp_pbf();
        let meta = |v: i32| ElementMeta {
            version: v,
            timestamp: 1_700_000_000 + i64::from(v),
            changeset: 1000 + i64::from(v),
            uid: 42 + v,
            user: format!("user{v}"),
        };
        let mut w = PbfWriter::new(
            std::fs::File::create(&path).expect("create"),
            &PbfWriterOptions::new(),
        )
        .expect("header");
        // A node without metadata before one with it: both must survive.
        w.write_node(1, FixedCoord::new(1, 1), &[]).expect("node");
        w.write_node_with_meta(2, FixedCoord::new(2, 2), &[("a", "b")], Some(&meta(3)))
            .expect("node");
        w.write_way_with_meta(10, &[1, 2], &[], Some(&meta(4)))
            .expect("way");
        w.write_relation_with_meta(20, &[(OsmType::Way, 10, "")], &[], Some(&meta(5)))
            .expect("relation");
        w.finish().expect("finish");
        let mut seen = Vec::new();
        ElementReader::from_path(&path)
            .expect("open")
            .for_each(|e| match e {
                Element::DenseNode(n) => {
                    let i = n.info().expect("dense info");
                    seen.push((
                        n.id(),
                        i.version(),
                        i.milli_timestamp() / 1000,
                        i.changeset(),
                        i.uid(),
                        i.user().expect("utf8").to_string(),
                    ));
                }
                Element::Way(w) => {
                    let i = w.info();
                    seen.push((
                        w.id(),
                        i.version().expect("v"),
                        i.milli_timestamp().expect("t") / 1000,
                        i.changeset().expect("c"),
                        i.uid().expect("u"),
                        i.user().expect("user").expect("utf8").to_string(),
                    ));
                }
                Element::Relation(r) => {
                    let i = r.info();
                    seen.push((
                        r.id(),
                        i.version().expect("v"),
                        i.milli_timestamp().expect("t") / 1000,
                        i.changeset().expect("c"),
                        i.uid().expect("u"),
                        i.user().expect("user").expect("utf8").to_string(),
                    ));
                }
                Element::Node(_) => panic!("dense only"),
            })
            .expect("read");
        let m = |id: i64, v: i32| {
            (
                id,
                v,
                1_700_000_000 + i64::from(v),
                1000 + i64::from(v),
                42 + v,
                format!("user{v}"),
            )
        };
        assert_eq!(seen[0], (1, 0, 0, 0, 0, String::new()), "no metadata: zero");
        assert_eq!(&seen[1..], [m(2, 3), m(10, 4), m(20, 5)]);
    }

    #[test]
    fn empty_tag_keys_do_not_break_dense_nodes() {
        let (_dir, path) = temp_pbf();
        let mut w = PbfWriter::new(
            std::fs::File::create(&path).expect("create"),
            &PbfWriterOptions::new(),
        )
        .expect("header");
        w.write_node(1, FixedCoord::new(0, 0), &[("", "x"), ("amenity", "cafe")])
            .expect("node");
        w.write_node(2, FixedCoord::new(0, 0), &[("name", "")])
            .expect("node");
        w.finish().expect("finish");
        let mut tags = Vec::new();
        ElementReader::from_path(&path)
            .expect("open")
            .for_each(|e| {
                if let Element::DenseNode(n) = e {
                    tags.push(
                        n.tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect::<Vec<_>>(),
                    );
                }
            })
            .expect("read");
        let t = |p: &[(&str, &str)]| {
            p.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            tags,
            [t(&[("", "x"), ("amenity", "cafe")]), t(&[("name", "")])]
        );
    }

    #[test]
    fn huge_blocks_are_split_and_stay_readable() {
        let (_dir, path) = temp_pbf();
        let mut w = PbfWriter::new(
            std::fs::File::create(&path).expect("create"),
            &PbfWriterOptions::new(),
        )
        .expect("header");
        // 8 000 ways of 2 000 refs would be ~70 MiB in one block.
        let refs: Vec<i64> = (0..2_000).map(|i| i * 1_000_003).collect();
        for id in 1..=8_000 {
            w.write_way(id, &refs, &[]).expect("way");
        }
        w.finish().expect("finish");
        let mut ways = 0;
        ElementReader::from_path(&path)
            .expect("open")
            .for_each(|e| {
                if let Element::Way(w) = e {
                    assert_eq!(w.refs().count(), 2_000);
                    ways += 1;
                }
            })
            .expect("readable by osmpbf");
        assert_eq!(ways, 8_000);
    }

    #[test]
    fn oversized_elements_are_rejected() {
        let mut w = PbfWriter::new(Vec::new(), &PbfWriterOptions::new()).expect("header");
        let huge = "x".repeat(MAX_ELEMENT_BYTES);
        let err = w
            .write_node(1, FixedCoord::new(0, 0), &[("k", &huge)])
            .expect_err("too big");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn locations_on_ways_round_trip() {
        let (_dir, path) = temp_pbf();
        let opts = PbfWriterOptions::new().locations_on_ways(true);
        let mut w =
            PbfWriter::new(std::fs::File::create(&path).expect("create"), &opts).expect("header");
        let a = FixedCoord::new(-1_000_000_000, 400_000_000);
        w.write_way_with_locations(1, &[1, 2], &[Some(a), None], &[], None)
            .expect("way");
        assert!(w.write_way_with_locations(2, &[1], &[], &[], None).is_err());
        w.finish().expect("finish");
        assert!(
            crate::pbf::read_header(&path)
                .expect("header")
                .has_locations_on_ways()
        );
        ElementReader::from_path(&path)
            .expect("open")
            .for_each(|e| {
                if let Element::Way(w) = e {
                    let locs: Vec<_> = w
                        .node_locations()
                        .map(|l| crate::pbf::location(l.nano_lon(), l.nano_lat()))
                        .collect();
                    assert_eq!(locs, [Some(a), None]);
                }
            })
            .expect("read");
    }

    #[test]
    fn checked_locations_reject_wrapping_and_missing() {
        assert_eq!(
            crate::pbf::location(100, -200),
            Some(FixedCoord::new(1, -2))
        );
        // 440° would wrap to a valid-looking latitude with `as i32`.
        assert_eq!(crate::pbf::location(0, 440_000_000_000), None);
        assert_eq!(crate::pbf::location(i64::from(i32::MAX) * 100, 0), None);
        assert_eq!(crate::pbf::location(181_000_000_000, 0), None);
    }
}
