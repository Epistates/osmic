//! OsmChange (`.osc`) parsing.
//!
//! Change files come from the network, so parsing is defensive:
//! - DTDs and entity references are rejected (XXE / billion laughs);
//! - attributes are read in one pass without quick-xml's duplicate check,
//!   which is quadratic in the attribute count (RUSTSEC-2026-0194);
//! - decompressed input is capped (gzip bombs);
//! - numeric attributes are parsed strictly — a malformed id or coordinate
//!   is an error, never silently `0`;
//! - `visible="false"` objects are treated as deletions.

use std::io::{BufRead, BufReader, Read};

use flate2::read::MultiGzDecoder;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use osmic_core::{FixedCoord, OsmId, OsmType};

use crate::error::ReplError;

/// The kind of change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAction {
    Create,
    Modify,
    Delete,
}

/// A relation member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub osm_type: OsmType,
    pub id: i64,
    pub role: String,
}

/// The new state of an object (absent for deletions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Element {
    Node {
        id: i64,
        location: FixedCoord,
        tags: Vec<(String, String)>,
    },
    Way {
        id: i64,
        refs: Vec<i64>,
        tags: Vec<(String, String)>,
    },
    Relation {
        id: i64,
        members: Vec<Member>,
        tags: Vec<(String, String)>,
    },
}

impl Element {
    pub fn osm_id(&self) -> OsmId {
        match self {
            Self::Node { id, .. } => OsmId::node(*id),
            Self::Way { id, .. } => OsmId::way(*id),
            Self::Relation { id, .. } => OsmId::relation(*id),
        }
    }
}

/// One change from a change file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub action: ChangeAction,
    pub id: OsmId,
    pub version: Option<u32>,
    /// The object after the change; `None` for deletions.
    pub element: Option<Element>,
}

/// Limits applied while reading change files.
#[derive(Debug, Clone, Copy)]
pub struct OscLimits {
    /// Maximum decompressed size of one change file.
    pub max_decompressed_bytes: u64,
}

impl Default for OscLimits {
    fn default() -> Self {
        // A daily planet diff is ~150 MB gzipped / ~1.5 GB of XML.
        Self {
            max_decompressed_bytes: 4 << 30,
        }
    }
}

/// Parse a gzip-compressed change file.
pub fn parse_osc_gz(data: impl Read, limits: OscLimits) -> Result<Vec<Change>, ReplError> {
    let limited = MultiGzDecoder::new(data).take(limits.max_decompressed_bytes + 1);
    let mut counting = CountingReader {
        inner: limited,
        read: 0,
    };
    let result = parse_osc(BufReader::new(&mut counting));
    if counting.read > limits.max_decompressed_bytes {
        return Err(ReplError::TooLarge {
            what: "decompressed change file",
            limit: limits.max_decompressed_bytes,
        });
    }
    result
}

struct CountingReader<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

struct Attrs<'a> {
    pairs: Vec<(&'a str, String)>,
}

impl Attrs<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    fn required(&self, element: &str, name: &str) -> Result<&str, ReplError> {
        self.get(name)
            .ok_or_else(|| ReplError::Osc(format!("<{element}> without `{name}`")))
    }

    fn parse<T: std::str::FromStr>(&self, element: &str, name: &str) -> Result<T, ReplError> {
        let raw = self.required(element, name)?;
        raw.parse()
            .map_err(|_| ReplError::Osc(format!("<{element}> has invalid {name}=\"{raw}\"")))
    }
}

/// Read every attribute once (no duplicate-name check).
fn attrs<'a>(e: &'a BytesStart<'_>) -> Result<Attrs<'a>, ReplError> {
    let mut pairs = Vec::new();
    for a in e.attributes().with_checks(false) {
        let a = a.map_err(|err| ReplError::Osc(err.to_string()))?;
        // Resolves predefined and character references only; general
        // entities surface as `Event::GeneralRef` and are rejected.
        let value = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|err| ReplError::Osc(err.to_string()))?
            .into_owned();
        pairs.push((a.key.into_inner(), value));
    }
    Ok(Attrs { pairs })
}

#[derive(Debug)]
struct Open {
    action: ChangeAction,
    id: OsmId,
    version: Option<u32>,
    visible: bool,
    location: Option<FixedCoord>,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    members: Vec<Member>,
}

impl Open {
    fn finish(self) -> Change {
        let deleted = self.action == ChangeAction::Delete || !self.visible;
        let element = if deleted {
            None
        } else {
            Some(match self.id.osm_type {
                OsmType::Node => Element::Node {
                    id: self.id.id,
                    // Presence is checked when the element opens.
                    location: self.location.unwrap_or(FixedCoord::new(0, 0)),
                    tags: self.tags,
                },
                OsmType::Way => Element::Way {
                    id: self.id.id,
                    refs: self.refs,
                    tags: self.tags,
                },
                OsmType::Relation => Element::Relation {
                    id: self.id.id,
                    members: self.members,
                    tags: self.tags,
                },
            })
        };
        Change {
            action: if deleted {
                ChangeAction::Delete
            } else {
                self.action
            },
            id: self.id,
            version: self.version,
            element,
        }
    }
}

fn coordinate(a: &Attrs<'_>, name: &str, limit: f64) -> Result<f64, ReplError> {
    let v: f64 = a.parse("node", name)?;
    if !v.is_finite() || v.abs() > limit {
        return Err(ReplError::Osc(format!("node {name}={v} out of range")));
    }
    Ok(v)
}

/// Parse an uncompressed change file.
pub fn parse_osc(reader: impl BufRead) -> Result<Vec<Change>, ReplError> {
    let mut xml = Reader::from_reader(reader);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut changes = Vec::new();
    let mut action: Option<ChangeAction> = None;
    let mut open: Option<Open> = None;

    loop {
        let event = xml.read_event_into(&mut buf).map_err(|e| {
            ReplError::Osc(format!("XML error at byte {}: {e}", xml.error_position()))
        })?;
        let is_empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = e.name();
                match name.as_ref() {
                    "create" => action = Some(ChangeAction::Create),
                    "modify" => action = Some(ChangeAction::Modify),
                    "delete" => action = Some(ChangeAction::Delete),
                    tag @ ("node" | "way" | "relation") => {
                        let Some(act) = action else {
                            return Err(ReplError::Osc(
                                "object outside <create>/<modify>/<delete>".into(),
                            ));
                        };
                        let a = attrs(e)?;
                        let (ty, label) = match tag {
                            "node" => (OsmType::Node, "node"),
                            "way" => (OsmType::Way, "way"),
                            _ => (OsmType::Relation, "relation"),
                        };
                        let id: i64 = a.parse(label, "id")?;
                        let visible = a.get("visible") != Some("false");
                        let version = a
                            .get("version")
                            .map(|_| a.parse(label, "version"))
                            .transpose()?;
                        let location =
                            if ty == OsmType::Node && act != ChangeAction::Delete && visible {
                                let lon = coordinate(&a, "lon", 180.0)?;
                                let lat = coordinate(&a, "lat", 90.0)?;
                                Some(FixedCoord::from_degrees(lon, lat).ok_or_else(|| {
                                    ReplError::Osc(format!("node {id} has invalid coordinates"))
                                })?)
                            } else {
                                None
                            };
                        let o = Open {
                            action: act,
                            id: OsmId::new(ty, id),
                            version,
                            visible,
                            location,
                            tags: Vec::new(),
                            refs: Vec::new(),
                            members: Vec::new(),
                        };
                        if is_empty {
                            changes.push(o.finish());
                        } else {
                            open = Some(o);
                        }
                    }
                    "tag" => {
                        let a = attrs(e)?;
                        if let Some(o) = open.as_mut() {
                            o.tags.push((
                                a.required("tag", "k")?.to_owned(),
                                a.required("tag", "v")?.to_owned(),
                            ));
                        }
                    }
                    "nd" => {
                        let a = attrs(e)?;
                        if let Some(o) = open.as_mut() {
                            o.refs.push(a.parse("nd", "ref")?);
                        }
                    }
                    "member" => {
                        let a = attrs(e)?;
                        if let Some(o) = open.as_mut() {
                            let osm_type = match a.required("member", "type")? {
                                "node" => OsmType::Node,
                                "way" => OsmType::Way,
                                "relation" => OsmType::Relation,
                                other => {
                                    return Err(ReplError::Osc(format!(
                                        "unknown member type {other:?}"
                                    )));
                                }
                            };
                            o.members.push(Member {
                                osm_type,
                                id: a.parse("member", "ref")?,
                                role: a.get("role").unwrap_or_default().to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match e.name().as_ref() {
                "create" | "modify" | "delete" => action = None,
                "node" | "way" | "relation" => {
                    if let Some(o) = open.take() {
                        changes.push(o.finish());
                    }
                }
                _ => {}
            },
            Event::DocType(_) => {
                return Err(ReplError::Osc("DTD declarations are not allowed".into()));
            }
            Event::GeneralRef(_) => {
                return Err(ReplError::Osc("entity references are not allowed".into()));
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if open.is_some() {
        return Err(ReplError::Osc("truncated change file".into()));
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(xml: &str) -> Result<Vec<Change>, ReplError> {
        parse_osc(xml.as_bytes())
    }

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" version="1" lat="37.7749295" lon="-122.4194155">
      <tag k="amenity" v="cafe"/>
      <tag k="name" v="Caf&#233; &amp; Bar"/>
    </node>
    <node id="2" version="1" lat="0" lon="0"/>
  </create>
  <modify>
    <way id="10" version="3">
      <nd ref="1"/><nd ref="2"/>
      <tag k="highway" v="residential"/>
    </way>
    <relation id="100" version="2">
      <member type="way" ref="10" role="outer"/>
      <member type="node" ref="1" role=""/>
      <tag k="type" v="multipolygon"/>
    </relation>
    <node id="3" version="4" visible="false"/>
  </modify>
  <delete>
    <node id="5" version="2"/>
    <way id="11" version="7"/>
  </delete>
</osmChange>"#;

    #[test]
    fn parses_every_kind_of_change_exactly() {
        let c = parse(SAMPLE).expect("valid");
        assert_eq!(c.len(), 7);
        let Some(Element::Node { location, tags, .. }) = c[0].element.clone() else {
            panic!("node");
        };
        assert_eq!(
            location,
            FixedCoord::new(-1_224_194_155, 377_749_295),
            "exact 1e-7"
        );
        assert_eq!(tags[1], ("name".to_string(), "Café & Bar".to_string()));
        assert_eq!(
            c[1].element,
            Some(Element::Node {
                id: 2,
                location: FixedCoord::new(0, 0),
                tags: vec![]
            })
        );
        assert!(matches!(&c[2].element, Some(Element::Way { refs, .. }) if refs == &[1, 2]));
        let Some(Element::Relation { members, .. }) = &c[3].element else {
            panic!("relation");
        };
        assert_eq!(
            members[0],
            Member {
                osm_type: OsmType::Way,
                id: 10,
                role: "outer".into()
            }
        );
        // visible="false" in a modify block is a deletion.
        assert_eq!(
            (c[4].action, c[4].element.as_ref()),
            (ChangeAction::Delete, None)
        );
        assert_eq!(c[5].id, OsmId::node(5));
        assert_eq!(c[6].id, OsmId::way(11));
        assert_eq!(c[6].version, Some(7));
    }

    #[test]
    fn malformed_numbers_are_errors_not_zero() {
        for bad in [
            r#"<osmChange><create><node id="x" lat="1" lon="1"/></create></osmChange>"#,
            r#"<osmChange><create><node id="1" lat="91" lon="1"/></create></osmChange>"#,
            r#"<osmChange><create><node id="1" lon="1"/></create></osmChange>"#,
            r#"<osmChange><create><node id="1" lat="NaN" lon="1"/></create></osmChange>"#,
            r#"<osmChange><modify><way id="1"><nd ref="abc"/></way></modify></osmChange>"#,
            r#"<osmChange><modify><relation id="1"><member type="area" ref="1" role=""/></relation></modify></osmChange>"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_dtd_entities_and_structure_errors() {
        assert!(parse(r#"<!DOCTYPE x [<!ENTITY a "b">]><osmChange/>"#).is_err());
        assert!(parse(r#"<osmChange><create><node id="1" lat="1" lon="1"><tag k="a" v="&a;"/></node></create></osmChange>"#).is_err());
        assert!(parse(r#"<osmChange><node id="1" lat="1" lon="1"/></osmChange>"#).is_err());
        assert!(parse(r#"<osmChange><create><node id="1" lat="1" lon="1">"#).is_err());
    }

    #[test]
    fn many_attributes_parse_in_linear_time() {
        // 20k attributes on one element: quadratic duplicate checking would
        // take seconds; a single pass is instant.
        let mut xml = String::from(r#"<osmChange><delete><node id="1""#);
        for i in 0..20_000 {
            xml.push_str(&format!(r#" a{i}="x""#));
        }
        xml.push_str("/></delete></osmChange>");
        let start = std::time::Instant::now();
        assert_eq!(parse(&xml).expect("valid").len(), 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn gzip_input_and_size_limit() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(SAMPLE.as_bytes()).expect("write");
        let bytes = gz.finish().expect("finish");
        assert_eq!(
            parse_osc_gz(&bytes[..], OscLimits::default())
                .expect("valid")
                .len(),
            7
        );
        let tiny = OscLimits {
            max_decompressed_bytes: 100,
        };
        assert!(matches!(
            parse_osc_gz(&bytes[..], tiny),
            Err(ReplError::TooLarge { .. })
        ));
    }
}
