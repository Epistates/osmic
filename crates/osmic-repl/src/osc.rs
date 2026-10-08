use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use osmic_core::error::{OsmicError, OsmicResult};

/// The type of change in an OSC file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAction {
    Create,
    Modify,
    Delete,
}

/// A parsed change from an OSC file.
#[derive(Debug, Clone)]
pub struct OscChange {
    pub action: ChangeAction,
    pub element: OscElement,
}

/// An OSM element from a change file.
#[derive(Debug, Clone)]
pub enum OscElement {
    Node {
        id: i64,
        lon: f64,
        lat: f64,
        version: u32,
        visible: bool,
        tags: Vec<(String, String)>,
    },
    Way {
        id: i64,
        version: u32,
        visible: bool,
        node_refs: Vec<i64>,
        tags: Vec<(String, String)>,
    },
    Relation {
        id: i64,
        version: u32,
        visible: bool,
        members: Vec<RelMember>,
        tags: Vec<(String, String)>,
    },
}

/// A relation member reference.
#[derive(Debug, Clone)]
pub struct RelMember {
    pub member_type: String,
    pub ref_id: i64,
    pub role: String,
}

/// Transient element-being-parsed state for the OSC XML state machine.
#[derive(Debug)]
enum Parsing {
    None,
    Node {
        id: i64,
        lon: f64,
        lat: f64,
        version: u32,
        visible: bool,
    },
    Way {
        id: i64,
        version: u32,
        visible: bool,
    },
    Relation {
        id: i64,
        version: u32,
        visible: bool,
    },
}

impl OscElement {
    pub fn id(&self) -> i64 {
        match self {
            OscElement::Node { id, .. } => *id,
            OscElement::Way { id, .. } => *id,
            OscElement::Relation { id, .. } => *id,
        }
    }
}

/// Parse an .osc.gz file from disk.
pub fn parse_osc_gz(path: &Path) -> OsmicResult<Vec<OscChange>> {
    let file = std::fs::File::open(path)
        .map_err(|e| OsmicError::Other(format!("Failed to open {}: {e}", path.display())))?;
    let decoder = GzDecoder::new(file);
    parse_osc(decoder)
}

/// Parse an .osc.gz from raw bytes (e.g. downloaded from replication server).
pub fn parse_osc_gz_bytes(data: &[u8]) -> OsmicResult<Vec<OscChange>> {
    let decoder = GzDecoder::new(data);
    parse_osc(decoder)
}

/// Parse OSC XML from any reader.
pub fn parse_osc<R: Read>(reader: R) -> OsmicResult<Vec<OscChange>> {
    let mut xml = Reader::from_reader(std::io::BufReader::new(reader));
    xml.config_mut().trim_text(true);

    let mut changes = Vec::new();
    let mut current_action: Option<ChangeAction> = None;
    let mut buf = Vec::new();

    // Transient state for the element being parsed
    let mut elem_tags: Vec<(String, String)> = Vec::new();
    let mut elem_nd_refs: Vec<i64> = Vec::new();
    let mut elem_members: Vec<RelMember> = Vec::new();
    let mut parsing = Parsing::None;

    loop {
        let event = xml.read_event_into(&mut buf);
        match event {
            // Start and Empty share the same "open element" handling. For Empty
            // (self-closing) elements we additionally finalize immediately since
            // quick-xml will NOT fire a matching End event.
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let is_empty = matches!(event, Ok(Event::Empty(_)));
                let qname = e.name();
                let name_bytes = qname.as_ref();
                let name = std::str::from_utf8(name_bytes).unwrap_or("");

                match name {
                    "create" => current_action = Some(ChangeAction::Create),
                    "modify" => current_action = Some(ChangeAction::Modify),
                    "delete" => current_action = Some(ChangeAction::Delete),
                    "node" => {
                        elem_tags.clear();
                        parsing = Parsing::Node {
                            id: attr_i64(e, "id"),
                            lon: attr_f64(e, "lon"),
                            lat: attr_f64(e, "lat"),
                            version: attr_u32(e, "version"),
                            visible: attr_bool(e, "visible"),
                        };
                    }
                    "way" => {
                        elem_tags.clear();
                        elem_nd_refs.clear();
                        parsing = Parsing::Way {
                            id: attr_i64(e, "id"),
                            version: attr_u32(e, "version"),
                            visible: attr_bool(e, "visible"),
                        };
                    }
                    "relation" => {
                        elem_tags.clear();
                        elem_members.clear();
                        parsing = Parsing::Relation {
                            id: attr_i64(e, "id"),
                            version: attr_u32(e, "version"),
                            visible: attr_bool(e, "visible"),
                        };
                    }
                    "tag" => {
                        let k = attr_str(e, "k");
                        let v = attr_str(e, "v");
                        elem_tags.push((k, v));
                    }
                    "nd" => {
                        elem_nd_refs.push(attr_i64(e, "ref"));
                    }
                    "member" => {
                        elem_members.push(RelMember {
                            member_type: attr_str(e, "type"),
                            ref_id: attr_i64(e, "ref"),
                            role: attr_str(e, "role"),
                        });
                    }
                    _ => {}
                }

                // Self-closing node/way/relation elements must be finalized here
                // because no End event will arrive.
                if is_empty && matches!(name, "node" | "way" | "relation") {
                    if let Some(action) = current_action
                        && let Some(element) = finalize_element(
                            &mut parsing,
                            &mut elem_tags,
                            &mut elem_nd_refs,
                            &mut elem_members,
                        )
                    {
                        changes.push(OscChange { action, element });
                    }
                    parsing = Parsing::None;
                }
            }
            Ok(Event::End(ref e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
                match name {
                    "create" | "modify" | "delete" => current_action = None,
                    "node" | "way" | "relation" => {
                        if let Some(action) = current_action
                            && let Some(element) = finalize_element(
                                &mut parsing,
                                &mut elem_tags,
                                &mut elem_nd_refs,
                                &mut elem_members,
                            )
                        {
                            changes.push(OscChange { action, element });
                        }
                        parsing = Parsing::None;
                    }
                    _ => {}
                }
            }
            // Reject DTDs outright — OSM replication files never contain them,
            // and permitting them opens the door to XXE and billion-laughs attacks.
            Ok(Event::DocType(_)) => {
                return Err(OsmicError::Other(
                    "DTD declarations are not allowed in OSC files".into(),
                ));
            }
            // Reject user-defined entity references for the same reason. The
            // five XML-predefined refs (&amp;lt; &amp;gt; &amp;amp; &amp;apos; &amp;quot;) and numeric
            // character references are handled by BytesText::unescape() and
            // never surface as GeneralRef events.
            Ok(Event::GeneralRef(_)) => {
                return Err(OsmicError::Other(
                    "Entity references are not allowed in OSC files".into(),
                ));
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(OsmicError::Other(format!("XML parse error: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(changes)
}

fn attr_str(e: &BytesStart, name: &str) -> String {
    // Unescape predefined XML entities (&amp; &lt; &gt; &apos; &quot; and numeric
    // character references). unescape_value() explicitly does NOT resolve
    // user-defined entities — those surface as Event::GeneralRef and are
    // rejected at the top of the parse loop for security.
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name.as_bytes())
        .and_then(|a| a.unescape_value().ok().map(|c| c.into_owned()))
        .unwrap_or_default()
}

fn attr_i64(e: &BytesStart, name: &str) -> i64 {
    attr_str(e, name).parse().unwrap_or(0)
}

fn attr_u32(e: &BytesStart, name: &str) -> u32 {
    attr_str(e, name).parse().unwrap_or(0)
}

fn attr_f64(e: &BytesStart, name: &str) -> f64 {
    attr_str(e, name).parse().unwrap_or(0.0)
}

fn attr_bool(e: &BytesStart, name: &str) -> bool {
    attr_str(e, name) != "false"
}

/// Consume the transient element-being-parsed state and produce the final
/// `OscElement`. Returns `None` if we aren't currently parsing an element
/// (e.g., a stray End event for an unknown element).
fn finalize_element(
    parsing: &mut Parsing,
    elem_tags: &mut Vec<(String, String)>,
    elem_nd_refs: &mut Vec<i64>,
    elem_members: &mut Vec<RelMember>,
) -> Option<OscElement> {
    match std::mem::replace(parsing, Parsing::None) {
        Parsing::Node {
            id,
            lon,
            lat,
            version,
            visible,
        } => Some(OscElement::Node {
            id,
            lon,
            lat,
            version,
            visible,
            tags: std::mem::take(elem_tags),
        }),
        Parsing::Way {
            id,
            version,
            visible,
        } => Some(OscElement::Way {
            id,
            version,
            visible,
            node_refs: std::mem::take(elem_nd_refs),
            tags: std::mem::take(elem_tags),
        }),
        Parsing::Relation {
            id,
            version,
            visible,
        } => Some(OscElement::Relation {
            id,
            version,
            visible,
            members: std::mem::take(elem_members),
            tags: std::mem::take(elem_tags),
        }),
        Parsing::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed minimal OSC file should parse without error.
    #[test]
    fn parse_minimal_osc() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6" generator="test">
    <create>
        <node id="1" lon="1.0" lat="2.0" version="1" visible="true"/>
    </create>
    <modify>
        <node id="2" lon="3.0" lat="4.0" version="2" visible="true">
            <tag k="name" v="test"/>
        </node>
    </modify>
    <delete>
        <node id="3" lon="5.0" lat="6.0" version="3" visible="false"/>
    </delete>
</osmChange>"#;
        let changes = parse_osc(&xml[..]).expect("valid OSC must parse");
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].action, ChangeAction::Create);
        assert_eq!(changes[1].action, ChangeAction::Modify);
        assert_eq!(changes[2].action, ChangeAction::Delete);
    }

    /// XXE attempt: DTD declaration with external entity referencing a local file.
    /// Must be rejected before any entity expansion or file I/O can occur.
    #[test]
    fn reject_xxe_external_entity() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE osmChange [
    <!ENTITY xxe SYSTEM "file:///etc/passwd">
]>
<osmChange version="0.6">
    <modify>
        <node id="1" lon="0" lat="0" version="1" visible="true">
            <tag k="name" v="&xxe;"/>
        </node>
    </modify>
</osmChange>"#;
        let err = parse_osc(&xml[..]).expect_err("XXE payload must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("DTD"),
            "error should mention DTD rejection, got: {msg}"
        );
    }

    /// Billion-laughs / quadratic blowup: nested DTD entity definitions.
    /// Must be rejected up-front at the DOCTYPE event, not allowed to expand.
    #[test]
    fn reject_billion_laughs() {
        let xml = br#"<?xml version="1.0"?>
<!DOCTYPE osmChange [
    <!ENTITY lol "lol">
    <!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
    <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
    <!ENTITY lol4 "&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;">
]>
<osmChange version="0.6">
    <create>
        <node id="1" lon="0" lat="0" version="1" visible="true">
            <tag k="name" v="&lol4;"/>
        </node>
    </create>
</osmChange>"#;
        let err = parse_osc(&xml[..]).expect_err("billion-laughs payload must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("DTD"),
            "error should mention DTD rejection, got: {msg}"
        );
    }

    /// The five predefined XML entities (lt, gt, amp, apos, quot) must still
    /// work — they're unescaped by quick-xml itself, never surface as events.
    #[test]
    fn predefined_entities_allowed() {
        let xml = br#"<?xml version="1.0"?>
<osmChange version="0.6">
    <create>
        <node id="1" lon="0" lat="0" version="1" visible="true">
            <tag k="name" v="Fish &amp; Chips"/>
        </node>
    </create>
</osmChange>"#;
        let changes = parse_osc(&xml[..]).expect("predefined entities must parse");
        assert_eq!(changes.len(), 1);
        if let OscElement::Node { tags, .. } = &changes[0].element {
            assert_eq!(tags.len(), 1);
            assert_eq!(tags[0].0, "name");
            assert_eq!(tags[0].1, "Fish & Chips");
        } else {
            panic!("expected Node, got {:?}", changes[0].element);
        }
    }
}
