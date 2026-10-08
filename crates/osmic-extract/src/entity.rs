//! Business entity extracted from OSM data.

use std::collections::BTreeMap;

use geo_types::Coord;
use serde::{Deserialize, Serialize};

use osmic_core::OsmType;

/// A named business entity extracted from OSM data with contact metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    /// Entity name (from `name` tag)
    pub name: String,
    /// OSM element type: "node", "way", or "relation"
    pub osm_type: String,
    /// OSM element ID
    pub osm_id: i64,
    /// Latitude (WGS84), `None` if no location could be determined
    pub lat: Option<f64>,
    /// Longitude (WGS84), `None` if no location could be determined
    pub lon: Option<f64>,
    /// Formatted address from addr:* tags (joined, human-readable).
    pub address: String,
    /// Phone number from phone/contact:phone/telephone tags
    pub phone: String,
    /// Website URL from website/contact:website/url tags
    pub website: String,
    /// Operator name from operator tag
    pub operator: String,
    /// Remaining tags as semicolon-separated key=value pairs
    pub tags: String,
    /// Structured address components sourced from raw `addr:*` tags.
    /// Keys are pre-prefixed (e.g. `addr_city`, `addr_housenumber`,
    /// `addr_street`) so they serialize as flat top-level fields via
    /// `#[serde(flatten)]`. Sub-colons are rewritten to underscores
    /// (`addr:street:name` → `addr_street_name`). CSV output ignores this
    /// field — structured fields are JSON/GeoJSON-only.
    #[serde(flatten)]
    pub address_parts: BTreeMap<String, String>,
}

fn get<'a>(tags: &[(&'a str, &'a str)], key: &str) -> &'a str {
    tags.iter().find(|(k, _)| *k == key).map_or("", |(_, v)| *v)
}

fn first_non_empty(tags: &[(&str, &str)], keys: &[&str]) -> String {
    keys.iter()
        .map(|k| get(tags, k))
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

impl Entity {
    /// Build an entity from an element's tags and location.
    pub fn new(
        osm_type: OsmType,
        osm_id: i64,
        location: Option<Coord<f64>>,
        tags: &[(&str, &str)],
    ) -> Self {
        Self {
            name: get(tags, "name").to_string(),
            osm_type: osm_type.as_str().to_string(),
            osm_id,
            lat: location.map(|c| c.y),
            lon: location.map(|c| c.x),
            address: Self::build_address(tags),
            phone: Self::extract_phone(tags),
            website: Self::extract_website(tags),
            operator: get(tags, "operator").to_string(),
            tags: Self::format_tags(tags),
            address_parts: Self::build_address_parts(tags),
        }
    }

    /// Sort rank of the element type (node, way, relation).
    pub fn osm_type_order(&self) -> u8 {
        match self.osm_type.as_str() {
            "node" => 0,
            "way" => 1,
            _ => 2,
        }
    }

    /// Canonical output order: element type, then id.
    pub fn sort_key(&self) -> (u8, i64) {
        (self.osm_type_order(), self.osm_id)
    }

    /// Build a formatted address from OSM addr:* tags.
    pub fn build_address(tags: &[(&str, &str)]) -> String {
        let mut parts = Vec::new();
        let housenumber = get(tags, "addr:housenumber");
        let street = get(tags, "addr:street");
        match (housenumber.is_empty(), street.is_empty()) {
            (false, false) => parts.push(format!("{housenumber} {street}")),
            (true, false) => parts.push(street.to_string()),
            _ => {}
        }
        for key in ["addr:city", "addr:state", "addr:postcode"] {
            let v = get(tags, key);
            if !v.is_empty() {
                parts.push(v.to_string());
            }
        }
        parts.join(", ")
    }

    /// Extract every `addr:*` tag into a map keyed by `addr_<suffix>`.
    /// Empty values are skipped; inner colons in the suffix become
    /// underscores so the keys are safe top-level JSON property names.
    pub fn build_address_parts(tags: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut parts = BTreeMap::new();
        for (k, v) in tags {
            let Some(suffix) = k.strip_prefix("addr:") else {
                continue;
            };
            if suffix.is_empty() || v.is_empty() {
                continue;
            }
            parts.insert(
                format!("addr_{}", suffix.replace(':', "_")),
                (*v).to_string(),
            );
        }
        parts
    }

    /// Extract phone number from OSM tag conventions.
    pub fn extract_phone(tags: &[(&str, &str)]) -> String {
        first_non_empty(tags, &["phone", "contact:phone", "telephone"])
    }

    /// Extract website URL from OSM tag conventions.
    pub fn extract_website(tags: &[(&str, &str)]) -> String {
        first_non_empty(tags, &["website", "contact:website", "url"])
    }

    /// Format remaining tags as semicolon-separated key=value pairs,
    /// excluding address, contact, and metadata fields.
    pub fn format_tags(tags: &[(&str, &str)]) -> String {
        const SKIP_KEYS: &[&str] = &["name", "phone", "telephone", "website", "url", "operator"];
        const SKIP_PREFIXES: &[&str] = &["addr:", "contact:"];
        let mut filtered: Vec<(&str, &str)> = tags
            .iter()
            .filter(|(k, _)| {
                !SKIP_KEYS.contains(k) && !SKIP_PREFIXES.iter().any(|p| k.starts_with(p))
            })
            .copied()
            .collect();
        filtered.sort_unstable();
        filtered
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// How much useful metadata the entity carries, used to choose the best
    /// of several duplicates (greater is richer).
    pub fn richness(&self) -> Richness {
        let filled = |s: &str| usize::from(!s.is_empty());
        let has_category = self.tag_pairs().any(|(k, _)| CATEGORY_KEYS.contains(&k));
        Richness {
            contact_fields: filled(&self.address) + filled(&self.phone) + filled(&self.website),
            completeness: filled(&self.name)
                + usize::from(has_category)
                + filled(&self.operator)
                + self.address_parts.len(),
            tag_count: self.tag_pairs().count(),
        }
    }

    /// The `key=value` pairs of [`tags`](Self::tags).
    fn tag_pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.tags
            .split("; ")
            .filter_map(|pair| pair.split_once('='))
    }
}

/// Keys that say what kind of place an entity is.
const CATEGORY_KEYS: &[&str] = &[
    "amenity",
    "craft",
    "healthcare",
    "leisure",
    "office",
    "shop",
    "tourism",
];

/// How much useful metadata an [`Entity`] carries, compared field by field
/// in declaration order: contact data outweighs everything else, however
/// many other tags an entity has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct Richness {
    /// Filled contact fields: address, phone and website.
    pub contact_fields: usize,
    /// Name, a category tag (`amenity`, `shop`, …), operator, and each
    /// structured `addr:*` component.
    pub completeness: usize,
    /// Number of remaining tags; only breaks ties.
    pub tag_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_parts_us_shape_full() {
        let tags = [
            ("addr:housenumber", "39"),
            ("addr:street", "East Southern Avenue"),
            ("addr:city", "Phoenix"),
            ("addr:state", "AZ"),
            ("addr:postcode", "85040"),
        ];
        let parts = Entity::build_address_parts(&tags);
        assert_eq!(
            parts.get("addr_housenumber").map(String::as_str),
            Some("39")
        );
        assert_eq!(parts.get("addr_city").map(String::as_str), Some("Phoenix"));
        assert_eq!(parts.len(), 5);
        assert_eq!(
            Entity::build_address(&tags),
            "39 East Southern Avenue, Phoenix, AZ, 85040"
        );
    }

    #[test]
    fn address_parts_skip_empty_and_rewrite_nested_colons() {
        let tags = [
            ("addr:city", "Phoenix"),
            ("addr:state", ""),
            ("addr:street:name", "Main"),
            ("name", "Midas"),
        ];
        let parts = Entity::build_address_parts(&tags);
        assert!(parts.contains_key("addr_city"));
        assert!(!parts.contains_key("addr_state"));
        assert_eq!(
            parts.get("addr_street_name").map(String::as_str),
            Some("Main")
        );
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn contact_fields_take_first_non_empty() {
        let tags = [
            ("phone", ""),
            ("contact:phone", "555"),
            ("url", "https://x"),
        ];
        assert_eq!(Entity::extract_phone(&tags), "555");
        assert_eq!(Entity::extract_website(&tags), "https://x");
    }

    #[test]
    fn contact_data_outranks_long_tags() {
        let entity = |tags: &[(&str, &str)]| Entity::new(OsmType::Node, 1, None, tags);
        let contact = entity(&[
            ("name", "Midas"),
            ("addr:street", "Main St"),
            ("phone", "+1 555"),
            ("website", "https://midas.example"),
        ]);
        let hours = "Mo-Fr 07:30-18:00; Sa 08:00-17:00; Su 09:00-15:00; PH off; ".repeat(10);
        let verbose = entity(&[
            ("name", "Midas"),
            ("shop", "car_repair"),
            ("opening_hours", &hours),
            ("brand", "Midas"),
            ("brand:wikidata", "Q3312613"),
        ]);
        assert!(contact.richness() > verbose.richness());
        assert_eq!(contact.richness().contact_fields, 3);

        // With equal contact data, a category and address detail count
        // before the number of tags.
        let categorised = entity(&[("name", "Midas"), ("phone", "1"), ("shop", "car_repair")]);
        let tagged = entity(&[("name", "Midas"), ("phone", "1"), ("a", "1"), ("b", "2")]);
        assert!(categorised.richness() > tagged.richness());
        let tie = entity(&[("name", "Midas"), ("phone", "1"), ("shop", "x"), ("c", "3")]);
        assert!(
            tie.richness() > categorised.richness(),
            "tag count breaks ties"
        );
    }

    #[test]
    fn new_fills_fields_and_serializes_addr_parts_flat() {
        let tags = [
            ("name", "Midas"),
            ("shop", "car_repair"),
            ("addr:city", "Phoenix"),
            ("operator", "Midas Inc"),
        ];
        let e = Entity::new(OsmType::Way, 7, Some(Coord { x: -112.0, y: 33.0 }), &tags);
        assert_eq!((e.osm_type.as_str(), e.osm_id), ("way", 7));
        assert_eq!((e.lat, e.lon), (Some(33.0), Some(-112.0)));
        assert_eq!(e.tags, "shop=car_repair");
        assert_eq!(e.operator, "Midas Inc");
        let v: serde_json::Value = serde_json::to_value(&e).expect("serialize");
        assert_eq!(v["addr_city"], "Phoenix");
        assert!(v.get("address").is_some());
    }
}
