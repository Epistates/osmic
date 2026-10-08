//! OsmChange (`.osc`) parsing.
//!
//! Change files come from the network, so parsing is defensive:
//! - the document must be an `<osmChange>` whose structure is checked as it
//!   streams (objects inside `<create>`/`<modify>`/`<delete>`, tags, node
//!   references and members only inside objects, no nesting, nothing left
//!   open at the end) — an error page or a truncated body is an error, not
//!   an empty diff;
//! - DTDs and entity references are rejected (XXE / billion laughs);
//! - attributes are read in one pass without quick-xml's duplicate check,
//!   which is quadratic in the attribute count (RUSTSEC-2026-0194), and
//!   duplicates of the attributes osmic reads are rejected;
//! - decompressed input and object sizes are capped ([`OscLimits`], by
//!   default the OSM API's limits);
//! - numeric attributes are parsed strictly — a malformed id or coordinate
//!   is an error, never silently `0`;
//! - `visible="false"` objects are treated as deletions.

use std::io::{BufRead, BufReader, Read};

use flate2::read::MultiGzDecoder;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use osmic_core::{FixedCoord, OsmId, OsmType};
use osmic_osm::pbf::ElementMeta;

use crate::error::ReplError;
use crate::state::parse_iso8601;

/// The kind of change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChangeAction {
    /// The object is new (`<create>`).
    Create,
    /// The object has a new version (`<modify>`).
    Modify,
    /// The object was deleted (`<delete>`, or `visible="false"` in any
    /// block).
    Delete,
}

/// A relation member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Type of the member object.
    pub osm_type: OsmType,
    /// Id of the member object, within `osm_type`.
    pub id: i64,
    /// The member's role; empty if it has none.
    pub role: String,
}

/// The new state of an object (absent for deletions).
///
/// `tags` are in document order; `meta` is `None` when the object carries
/// none of `version`, `timestamp`, `changeset` or `uid`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Element {
    /// A node.
    Node {
        /// Node id.
        id: i64,
        /// Location, from the `lon`/`lat` attributes.
        location: FixedCoord,
        /// `(key, value)` tags.
        tags: Vec<(String, String)>,
        /// Version, timestamp, changeset and user, if present.
        meta: Option<ElementMeta>,
    },
    /// A way.
    Way {
        /// Way id.
        id: i64,
        /// Node ids, in order.
        refs: Vec<i64>,
        /// `(key, value)` tags.
        tags: Vec<(String, String)>,
        /// Version, timestamp, changeset and user, if present.
        meta: Option<ElementMeta>,
    },
    /// A relation.
    Relation {
        /// Relation id.
        id: i64,
        /// Members, in order.
        members: Vec<Member>,
        /// `(key, value)` tags.
        tags: Vec<(String, String)>,
        /// Version, timestamp, changeset and user, if present.
        meta: Option<ElementMeta>,
    },
}

impl Element {
    /// The object's typed id.
    pub fn osm_id(&self) -> OsmId {
        match self {
            Self::Node { id, .. } => OsmId::node(*id),
            Self::Way { id, .. } => OsmId::way(*id),
            Self::Relation { id, .. } => OsmId::relation(*id),
        }
    }

    /// The object's metadata, if the change file carried it.
    pub fn meta(&self) -> Option<&ElementMeta> {
        match self {
            Self::Node { meta, .. } | Self::Way { meta, .. } | Self::Relation { meta, .. } => {
                meta.as_ref()
            }
        }
    }
}

/// One change from a change file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What happened; [`ChangeAction::Delete`] whenever `element` is `None`.
    pub action: ChangeAction,
    /// The object changed.
    pub id: OsmId,
    /// The object's version after the change, if the file gives one; used
    /// to order repeated changes (see [`ChangeSet`](crate::ChangeSet)).
    pub version: Option<u32>,
    /// The object after the change; `None` for deletions.
    pub element: Option<Element>,
}

/// Limits applied while reading change files. The defaults are the OSM
/// API's own limits, so genuine data never hits them.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct OscLimits {
    /// Maximum decompressed size of one change file.
    pub max_decompressed_bytes: u64,
    /// Maximum length of a tag key, tag value or member role, in characters.
    pub max_string_chars: usize,
    /// Maximum tags on one object.
    pub max_tags: usize,
    /// Maximum nodes in one way.
    pub max_way_nodes: usize,
    /// Maximum members of one relation.
    pub max_relation_members: usize,
}

impl Default for OscLimits {
    fn default() -> Self {
        // A daily planet diff is ~150 MB gzipped / ~1.5 GB of XML.
        Self {
            max_decompressed_bytes: 4 << 30,
            max_string_chars: 255,
            max_tags: 5_000,
            max_way_nodes: 2_000,
            max_relation_members: 32_000,
        }
    }
}

/// Parse a gzip-compressed change file.
///
/// # Errors
///
/// As for [`parse_osc_gz_with`].
pub fn parse_osc_gz(data: impl Read, limits: OscLimits) -> Result<Vec<Change>, ReplError> {
    let mut changes = Vec::new();
    parse_osc_gz_with(data, limits, |c| {
        changes.push(c);
        Ok(())
    })?;
    Ok(changes)
}

/// Parse a gzip-compressed change file, handing each change to `sink` as
/// it is read.
///
/// # Errors
///
/// [`ReplError::TooLarge`] if the file decompresses to more than
/// [`OscLimits::max_decompressed_bytes`]; otherwise as for
/// [`parse_osc_with`], with decompression failures reported as
/// [`ReplError::Osc`].
pub fn parse_osc_gz_with(
    data: impl Read,
    limits: OscLimits,
    sink: impl FnMut(Change) -> Result<(), ReplError>,
) -> Result<(), ReplError> {
    // One byte past the limit tells "exactly at the limit" from "over it".
    let limited = MultiGzDecoder::new(data).take(limits.max_decompressed_bytes.saturating_add(1));
    let mut counting = CountingReader {
        inner: limited,
        read: 0,
    };
    let result = parse_osc_with(BufReader::new(&mut counting), limits, sink);
    if counting.read > limits.max_decompressed_bytes {
        return Err(ReplError::TooLarge {
            what: "decompressed change file",
            limit: limits.max_decompressed_bytes,
        });
    }
    result
}

/// Parse a change file that is either gzip-compressed or plain XML (some
/// servers send `.osc.gz` with `Content-Encoding: gzip`, so the HTTP client
/// has already inflated it).
///
/// # Errors
///
/// As for [`parse_osc_gz_with`]; plain input longer than
/// [`OscLimits::max_decompressed_bytes`] is [`ReplError::TooLarge`] too.
pub fn parse_osc_auto_with(
    data: &[u8],
    limits: OscLimits,
    sink: impl FnMut(Change) -> Result<(), ReplError>,
) -> Result<(), ReplError> {
    if data.starts_with(&[0x1f, 0x8b]) {
        parse_osc_gz_with(data, limits, sink)
    } else {
        if data.len() as u64 > limits.max_decompressed_bytes {
            return Err(ReplError::TooLarge {
                what: "decompressed change file",
                limit: limits.max_decompressed_bytes,
            });
        }
        parse_osc_with(data, limits, sink)
    }
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

/// Attributes osmic reads; a duplicate of any of them is an error.
const KNOWN_ATTRIBUTES: &[&str] = &[
    "id",
    "version",
    "timestamp",
    "changeset",
    "uid",
    "user",
    "visible",
    "lat",
    "lon",
    "k",
    "v",
    "ref",
    "type",
    "role",
];

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

    fn optional<T: std::str::FromStr>(
        &self,
        element: &str,
        name: &str,
    ) -> Result<Option<T>, ReplError> {
        self.get(name)
            .map(|_| self.parse(element, name))
            .transpose()
    }
}

/// Read every attribute once; reject duplicates of known attributes.
fn attrs<'a>(e: &'a BytesStart<'_>) -> Result<Attrs<'a>, ReplError> {
    let mut pairs = Vec::new();
    let mut seen = [false; KNOWN_ATTRIBUTES.len()];
    for a in e.attributes().with_checks(false) {
        let a = a.map_err(|err| ReplError::Osc(err.to_string()))?;
        let key = a.key.into_inner();
        if let Some(i) = KNOWN_ATTRIBUTES.iter().position(|k| *k == key) {
            if seen[i] {
                return Err(ReplError::Osc(format!("duplicate attribute `{key}`")));
            }
            seen[i] = true;
        }
        // Resolves predefined and character references only; general
        // entities surface as `Event::GeneralRef` and are rejected.
        let value = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|err| ReplError::Osc(err.to_string()))?
            .into_owned();
        pairs.push((key, value));
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
    meta: Option<ElementMeta>,
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
                    meta: self.meta,
                },
                OsmType::Way => Element::Way {
                    id: self.id.id,
                    refs: self.refs,
                    tags: self.tags,
                    meta: self.meta,
                },
                OsmType::Relation => Element::Relation {
                    id: self.id.id,
                    members: self.members,
                    tags: self.tags,
                    meta: self.meta,
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

/// Metadata from an object's attributes, if it has any.
fn meta(a: &Attrs<'_>, element: &str) -> Result<Option<ElementMeta>, ReplError> {
    let version: Option<i32> = a.optional(element, "version")?;
    let timestamp =
        match a.get("timestamp") {
            Some(t) => Some(parse_iso8601(t).ok_or_else(|| {
                ReplError::Osc(format!("<{element}> has invalid timestamp=\"{t}\""))
            })?),
            None => None,
        };
    let changeset: Option<i64> = a.optional(element, "changeset")?;
    let uid: Option<i32> = a.optional(element, "uid")?;
    let user = a.get("user");
    if version.is_none() && timestamp.is_none() && changeset.is_none() && uid.is_none() {
        return Ok(None);
    }
    Ok(Some(ElementMeta {
        version: version.unwrap_or(0),
        timestamp: timestamp.unwrap_or(0),
        changeset: changeset.unwrap_or(0),
        uid: uid.unwrap_or(0),
        user: user.unwrap_or_default().to_owned(),
    }))
}

/// Where the parser is in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Inside `<osmChange>`.
    Root,
    /// Inside `<create>`, `<modify>` or `<delete>`.
    Action(ChangeAction),
    /// Inside a `<node>`, `<way>` or `<relation>`.
    Object,
    /// Inside a `<tag>`, `<nd>` or `<member>` written with an end tag.
    Child,
    /// Inside an element osmic does not use (skipped with its content).
    Ignored,
}

fn check_len(limits: &OscLimits, what: &str, value: &str) -> Result<(), ReplError> {
    if value.chars().count() > limits.max_string_chars {
        return Err(ReplError::Osc(format!(
            "{what} longer than {} characters",
            limits.max_string_chars
        )));
    }
    Ok(())
}

/// Parse an uncompressed change file with the default limits.
///
/// # Errors
///
/// As for [`parse_osc_with`].
pub fn parse_osc(reader: impl BufRead) -> Result<Vec<Change>, ReplError> {
    let mut changes = Vec::new();
    parse_osc_with(reader, OscLimits::default(), |c| {
        changes.push(c);
        Ok(())
    })?;
    Ok(changes)
}

/// Parse an uncompressed change file, handing each change to `sink` as it
/// is read.
///
/// [`OscLimits::max_decompressed_bytes`] is not applied here; bound
/// `reader` yourself if the input is untrusted.
///
/// # Errors
///
/// [`ReplError::Osc`] if reading fails, the document is malformed or not a
/// valid `<osmChange>` (see the module docs), or an object exceeds an
/// [`OscLimits`] count or length; any error returned by `sink`. Changes
/// read before the error have already been passed to `sink`.
pub fn parse_osc_with(
    reader: impl BufRead,
    limits: OscLimits,
    mut sink: impl FnMut(Change) -> Result<(), ReplError>,
) -> Result<(), ReplError> {
    let mut xml = Reader::from_reader(reader);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<Level> = Vec::new();
    let mut seen_root = false;
    let mut open: Option<Open> = None;
    let err = |m: &str| Err(ReplError::Osc(m.to_string()));

    loop {
        let event = xml.read_event_into(&mut buf).map_err(|e| {
            ReplError::Osc(format!("XML error at byte {}: {e}", xml.error_position()))
        })?;
        let is_empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = e.name();
                let name = name.as_ref();
                let level = match (stack.last().copied(), name) {
                    (None, "osmChange") if !seen_root => {
                        seen_root = true;
                        Level::Root
                    }
                    (None, _) => return err("not an <osmChange> document"),
                    (Some(Level::Ignored), _) => Level::Ignored,
                    (Some(Level::Root), "create") => Level::Action(ChangeAction::Create),
                    (Some(Level::Root), "modify") => Level::Action(ChangeAction::Modify),
                    (Some(Level::Root), "delete") => Level::Action(ChangeAction::Delete),
                    (Some(Level::Root | Level::Action(_)), "tag" | "nd" | "member") => {
                        return err("<tag>, <nd> or <member> outside an object");
                    }
                    (Some(Level::Root), "node" | "way" | "relation") => {
                        return err("object outside <create>/<modify>/<delete>");
                    }
                    (Some(Level::Action(act)), tag @ ("node" | "way" | "relation")) => {
                        let a = attrs(e)?;
                        let (ty, label) = match tag {
                            "node" => (OsmType::Node, "node"),
                            "way" => (OsmType::Way, "way"),
                            _ => (OsmType::Relation, "relation"),
                        };
                        let id: i64 = a.parse(label, "id")?;
                        let visible = a.get("visible") != Some("false");
                        let version: Option<u32> = a.optional(label, "version")?;
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
                            meta: meta(&a, label)?,
                            tags: Vec::new(),
                            refs: Vec::new(),
                            members: Vec::new(),
                        };
                        if is_empty {
                            sink(o.finish())?;
                        } else {
                            open = Some(o);
                        }
                        Level::Object
                    }
                    (Some(Level::Object), "node" | "way" | "relation") => {
                        return err("objects cannot be nested");
                    }
                    (Some(Level::Object), child @ ("tag" | "nd" | "member")) => {
                        let a = attrs(e)?;
                        let Some(o) = open.as_mut() else {
                            return err("child element without an open object");
                        };
                        match child {
                            "tag" => {
                                let (k, v) = (a.required("tag", "k")?, a.required("tag", "v")?);
                                check_len(&limits, "tag key", k)?;
                                check_len(&limits, "tag value", v)?;
                                if o.tags.len() >= limits.max_tags {
                                    return err("too many tags on one object");
                                }
                                o.tags.push((k.to_owned(), v.to_owned()));
                            }
                            "nd" => {
                                if o.refs.len() >= limits.max_way_nodes {
                                    return err("too many nodes in one way");
                                }
                                o.refs.push(a.parse("nd", "ref")?);
                            }
                            _ => {
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
                                let role = a.get("role").unwrap_or_default();
                                check_len(&limits, "member role", role)?;
                                if o.members.len() >= limits.max_relation_members {
                                    return err("too many members in one relation");
                                }
                                o.members.push(Member {
                                    osm_type,
                                    id: a.parse("member", "ref")?,
                                    role: role.to_owned(),
                                });
                            }
                        }
                        Level::Child
                    }
                    (Some(Level::Child), _) => return err("unexpected element inside a child"),
                    // Unknown elements (e.g. <bounds>) are skipped.
                    (Some(_), _) => Level::Ignored,
                };
                if !is_empty {
                    stack.push(level);
                }
            }
            Event::End(_) => match stack.pop() {
                Some(Level::Object) => {
                    if let Some(o) = open.take() {
                        sink(o.finish())?;
                    }
                }
                Some(_) => {}
                None => return err("unbalanced end tag"),
            },
            Event::DocType(_) => {
                return err("DTD declarations are not allowed");
            }
            Event::GeneralRef(_) => {
                return err("entity references are not allowed");
            }
            Event::Text(ref t) if stack.last() != Some(&Level::Ignored) => {
                if !t.trim().is_empty() {
                    return err("unexpected text content");
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !seen_root {
        return err("not an <osmChange> document");
    }
    if !stack.is_empty() || open.is_some() {
        return err("truncated change file");
    }
    Ok(())
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
                tags: vec![],
                meta: Some(ElementMeta {
                    version: 1,
                    ..Default::default()
                }),
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
    fn documents_must_be_well_formed_change_files() {
        for bad in [
            // Not a change file at all (an HTML error page, an empty body).
            "<html><body>502 Bad Gateway</body></html>",
            "",
            "   ",
            // Truncated.
            r#"<osmChange><create><node id="1" lat="1" lon="1"/></create>"#,
            r#"<osmChange><create><node id="1" lat="1" lon="1"/>"#,
            // Nested objects, stray children, text.
            r#"<osmChange><create><node id="1" lat="1" lon="1"><node id="2" lat="1" lon="1"/></node></create></osmChange>"#,
            r#"<osmChange><create><tag k="a" v="b"/></create></osmChange>"#,
            r#"<osmChange><tag k="a" v="b"/></osmChange>"#,
            r#"<osmChange>hello</osmChange>"#,
            // Duplicate attributes osmic reads.
            r#"<osmChange><delete><node id="1" id="2"/></delete></osmChange>"#,
            r#"<osmChange><create><node id="1" lat="1" lon="1"><tag k="a" k="b" v="c"/></node></create></osmChange>"#,
            // Bad metadata.
            r#"<osmChange><delete><node id="1" timestamp="yesterday"/></delete></osmChange>"#,
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        // Unknown elements are skipped; an empty document is a valid empty diff.
        assert_eq!(parse("<osmChange/>").expect("valid").len(), 0);
        let with_bounds = r#"<osmChange><bounds minlat="0"><extra/></bounds><delete><node id="1"/></delete></osmChange>"#;
        assert_eq!(parse(with_bounds).expect("valid").len(), 1);
    }

    #[test]
    fn metadata_is_read() {
        let c = parse(
            r#"<osmChange><modify><way id="7" version="3" timestamp="2026-10-08T12:00:00Z" changeset="99" uid="5" user="me &amp; you"><nd ref="1"/><nd ref="2"/></way></modify></osmChange>"#,
        )
        .expect("valid");
        assert_eq!(
            c[0].element.as_ref().and_then(Element::meta),
            Some(&ElementMeta {
                version: 3,
                timestamp: 1_791_460_800,
                changeset: 99,
                uid: 5,
                user: "me & you".into(),
            })
        );
    }

    #[test]
    fn osm_api_limits_are_enforced() {
        let long = "x".repeat(256);
        let tag = format!(
            r#"<osmChange><create><node id="1" lat="1" lon="1"><tag k="a" v="{long}"/></node></create></osmChange>"#
        );
        assert!(parse(&tag).is_err());
        let ok = format!(
            r#"<osmChange><create><node id="1" lat="1" lon="1"><tag k="a" v="{}"/></node></create></osmChange>"#,
            "é".repeat(255)
        );
        assert!(parse(&ok).is_ok(), "255 characters (not bytes) is fine");
        let nodes: String = (0..2_001).map(|i| format!(r#"<nd ref="{i}"/>"#)).collect();
        let way = format!(r#"<osmChange><create><way id="1">{nodes}</way></create></osmChange>"#);
        assert!(parse(&way).is_err());
    }

    #[test]
    fn plain_or_gzip_bodies_are_both_accepted() {
        use std::io::Write;
        let mut changes = 0;
        parse_osc_auto_with(SAMPLE.as_bytes(), OscLimits::default(), |_| {
            changes += 1;
            Ok(())
        })
        .expect("plain");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(SAMPLE.as_bytes()).expect("write");
        let bytes = gz.finish().expect("finish");
        parse_osc_auto_with(&bytes, OscLimits::default(), |_| {
            changes += 1;
            Ok(())
        })
        .expect("gzip");
        assert_eq!(changes, 14);
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
            ..OscLimits::default()
        };
        assert!(matches!(
            parse_osc_gz(&bytes[..], tiny),
            Err(ReplError::TooLarge { .. })
        ));
        // An unlimited size must not overflow the read-ahead byte.
        let unlimited = OscLimits {
            max_decompressed_bytes: u64::MAX,
            ..OscLimits::default()
        };
        assert_eq!(parse_osc_gz(&bytes[..], unlimited).expect("valid").len(), 7);
    }
}
