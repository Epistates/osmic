//! Minimal, spec-conformant OSM PBF writer.
//!
//! Writes an `OSMHeader` block followed by zlib-compressed `OSMData` blocks.
//! Each block holds one element type (dense nodes, ways or relations) and at
//! most 8 000 entities, as the format recommends. Coordinates use the default
//! granularity (100 nanodegrees = 1e-7°), so [`FixedCoord`] values are
//! written exactly. Elements must be written nodes, then ways, then
//! relations for the file to be flagged `Sort.Type_then_ID` (when also
//! sorted by id, see [`PbfWriterOptions::sorted`]).
//!
//! Metadata (version, timestamp, user) is not written.

use std::collections::HashMap;
use std::io::{self, Write};

use flate2::Compression;
use flate2::write::ZlibEncoder;

use osmic_core::{BBox, FixedCoord, OsmType};

const MAX_ENTITIES_PER_BLOCK: usize = 8_000;

/// Header options for [`PbfWriter`].
#[derive(Debug, Clone, Default)]
pub struct PbfWriterOptions {
    /// Bounding box written to the header.
    pub bbox: Option<BBox>,
    /// Declare `Sort.Type_then_ID`. Only set this if elements are written
    /// in type-then-id order.
    pub sorted: bool,
    /// `writingprogram` header field (defaults to `osmic`).
    pub writing_program: Option<String>,
    /// Additional `required_features` to declare (e.g. to produce test
    /// files that readers must reject).
    pub extra_required_features: Vec<String>,
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
    group: Option<Group>,
    strings: StringTable,
    count: usize,
    // Dense nodes.
    ids: Vec<i64>,
    lats: Vec<i64>,
    lons: Vec<i64>,
    keys_vals: Vec<u32>,
    // Ways / relations, already encoded.
    elements: Vec<u8>,
}

impl<W: Write> PbfWriter<W> {
    /// Start a file: writes the header block immediately.
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
        let program = options.writing_program.as_deref().unwrap_or("osmic");
        put_bytes(&mut header, 16, program.as_bytes());
        write_blob(&mut out, "OSMHeader", &header)?;
        Ok(Self {
            out,
            group: None,
            strings: StringTable::default(),
            count: 0,
            ids: Vec::new(),
            lats: Vec::new(),
            lons: Vec::new(),
            keys_vals: Vec::new(),
            elements: Vec::new(),
        })
    }

    fn begin(&mut self, group: Group) -> io::Result<()> {
        if self.group != Some(group) || self.count >= MAX_ENTITIES_PER_BLOCK {
            self.flush_block()?;
            self.group = Some(group);
        }
        self.count += 1;
        Ok(())
    }

    /// Write a node.
    pub fn write_node(
        &mut self,
        id: i64,
        coord: FixedCoord,
        tags: &[(&str, &str)],
    ) -> io::Result<()> {
        self.begin(Group::Nodes)?;
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
    pub fn write_way(&mut self, id: i64, refs: &[i64], tags: &[(&str, &str)]) -> io::Result<()> {
        self.begin(Group::Ways)?;
        let mut way = Vec::new();
        put_int64(&mut way, 1, id);
        self.put_tags(&mut way, tags);
        put_packed_delta_sint64(&mut way, 8, refs);
        put_bytes(&mut self.elements, 3, &way);
        Ok(())
    }

    /// Write a relation; members are `(type, id, role)`.
    pub fn write_relation(
        &mut self,
        id: i64,
        members: &[(OsmType, i64, &str)],
        tags: &[(&str, &str)],
    ) -> io::Result<()> {
        self.begin(Group::Relations)?;
        let mut rel = Vec::new();
        put_int64(&mut rel, 1, id);
        self.put_tags(&mut rel, tags);
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

    fn flush_block(&mut self) -> io::Result<()> {
        let Some(group) = self.group.take() else {
            return Ok(());
        };
        let mut group_buf = Vec::new();
        match group {
            Group::Nodes => {
                let mut dense = Vec::new();
                put_packed_delta_sint64(&mut dense, 1, &self.ids);
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
        write_blob(&mut self.out, "OSMData", &block)?;

        self.strings = StringTable::default();
        self.count = 0;
        self.ids.clear();
        self.lats.clear();
        self.lons.clear();
        self.keys_vals.clear();
        self.elements.clear();
        Ok(())
    }

    /// Flush the last block and return the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_block()?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Per-block string table; index 0 is the mandatory empty string.
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
        if s.is_empty() {
            return 0;
        }
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        let i = self.strings.len() as u32;
        self.strings.push(s.to_string());
        self.index.insert(s.to_string(), i);
        i
    }
}

fn write_blob(out: &mut impl Write, kind: &str, payload: &[u8]) -> io::Result<()> {
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
    out.write_all(&header_len.to_be_bytes())?;
    out.write_all(&header)?;
    out.write_all(&blob)
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
        let opts = PbfWriterOptions {
            sorted: true,
            ..Default::default()
        };
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
}
