//! Business entity extracted from OSM data.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A named business entity extracted from OSM data with contact metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    /// Entity name (from `name` tag)
    pub name: String,
    /// OSM element type: "node", "way", or "relation"
    pub osm_type: String,
    /// OSM element ID
    pub osm_id: i64,
    /// Latitude (WGS84), None if coordinates unavailable (some relations)
    pub lat: Option<f64>,
    /// Longitude (WGS84), None if coordinates unavailable
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

impl Entity {
    /// Build a formatted address from OSM addr:* tags.
    pub fn build_address(tags: &[(String, String)]) -> String {
        let get = |key: &str| -> &str {
            tags.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };

        let mut parts = Vec::new();
        let housenumber = get("addr:housenumber");
        let street = get("addr:street");
        match (housenumber.is_empty(), street.is_empty()) {
            (false, false) => parts.push(format!("{housenumber} {street}")),
            (true, false) => parts.push(street.to_string()),
            _ => {}
        }

        let city = get("addr:city");
        let state = get("addr:state");
        let postcode = get("addr:postcode");
        if !city.is_empty() {
            parts.push(city.to_string());
        }
        if !state.is_empty() {
            parts.push(state.to_string());
        }
        if !postcode.is_empty() {
            parts.push(postcode.to_string());
        }

        parts.join(", ")
    }

    /// Extract every `addr:*` tag into a map keyed by `addr_<suffix>`.
    /// Empty values are skipped; inner colons in the suffix become
    /// underscores so the keys are safe top-level JSON property names.
    pub fn build_address_parts(tags: &[(String, String)]) -> BTreeMap<String, String> {
        let mut parts = BTreeMap::new();
        for (k, v) in tags {
            let Some(suffix) = k.strip_prefix("addr:") else {
                continue;
            };
            if suffix.is_empty() || v.is_empty() {
                continue;
            }
            let key = format!("addr_{}", suffix.replace(':', "_"));
            parts.insert(key, v.clone());
        }
        parts
    }

    /// Extract phone number from OSM tag conventions.
    pub fn extract_phone(tags: &[(String, String)]) -> String {
        for key in &["phone", "contact:phone", "telephone"] {
            if let Some((_, v)) = tags.iter().find(|(k, _)| k == key)
                && !v.is_empty()
            {
                return v.clone();
            }
        }
        String::new()
    }

    /// Extract website URL from OSM tag conventions.
    pub fn extract_website(tags: &[(String, String)]) -> String {
        for key in &["website", "contact:website", "url"] {
            if let Some((_, v)) = tags.iter().find(|(k, _)| k == key)
                && !v.is_empty()
            {
                return v.clone();
            }
        }
        String::new()
    }

    /// Format remaining tags as semicolon-separated key=value pairs,
    /// excluding address, contact, and metadata fields.
    pub fn format_tags(tags: &[(String, String)]) -> String {
        let skip_keys: &[&str] = &["name", "phone", "telephone", "website", "url", "operator"];
        let skip_prefixes: &[&str] = &["addr:", "contact:"];

        let mut filtered: Vec<(&str, &str)> = tags
            .iter()
            .filter(|(k, _)| {
                !skip_keys.contains(&k.as_str()) && !skip_prefixes.iter().any(|p| k.starts_with(p))
            })
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        filtered.sort_by_key(|(k, _)| *k);
        filtered
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn address_parts_us_shape_full() {
        let tags = t(&[
            ("addr:housenumber", "39"),
            ("addr:street", "East Southern Avenue"),
            ("addr:city", "Phoenix"),
            ("addr:state", "AZ"),
            ("addr:postcode", "85040"),
        ]);
        let parts = Entity::build_address_parts(&tags);
        assert_eq!(
            parts.get("addr_housenumber").map(String::as_str),
            Some("39")
        );
        assert_eq!(
            parts.get("addr_street").map(String::as_str),
            Some("East Southern Avenue")
        );
        assert_eq!(parts.get("addr_city").map(String::as_str), Some("Phoenix"));
        assert_eq!(parts.get("addr_state").map(String::as_str), Some("AZ"));
        assert_eq!(
            parts.get("addr_postcode").map(String::as_str),
            Some("85040")
        );
        assert_eq!(parts.len(), 5);
    }

    #[test]
    fn address_parts_skips_empty_values() {
        let tags = t(&[
            ("addr:city", "Phoenix"),
            ("addr:state", ""),
            ("addr:postcode", "85040"),
        ]);
        let parts = Entity::build_address_parts(&tags);
        assert!(parts.contains_key("addr_city"));
        assert!(!parts.contains_key("addr_state"));
        assert!(parts.contains_key("addr_postcode"));
    }

    #[test]
    fn address_parts_passes_through_uncommon_keys() {
        // Non-US shapes: suburb, province, country.
        let tags = t(&[
            ("addr:street", "Königsallee"),
            ("addr:suburb", "Stadtmitte"),
            ("addr:city", "Düsseldorf"),
            ("addr:country", "DE"),
        ]);
        let parts = Entity::build_address_parts(&tags);
        assert_eq!(
            parts.get("addr_suburb").map(String::as_str),
            Some("Stadtmitte")
        );
        assert_eq!(parts.get("addr_country").map(String::as_str), Some("DE"));
    }

    #[test]
    fn address_parts_nested_colon_becomes_underscore() {
        let tags = t(&[("addr:street:name", "Main"), ("addr:street:type", "St")]);
        let parts = Entity::build_address_parts(&tags);
        assert_eq!(
            parts.get("addr_street_name").map(String::as_str),
            Some("Main")
        );
        assert_eq!(
            parts.get("addr_street_type").map(String::as_str),
            Some("St")
        );
    }

    #[test]
    fn address_parts_ignores_non_addr_tags() {
        let tags = t(&[
            ("name", "Midas"),
            ("phone", "555-1234"),
            ("addr:city", "Phoenix"),
        ]);
        let parts = Entity::build_address_parts(&tags);
        assert_eq!(parts.len(), 1);
        assert!(parts.contains_key("addr_city"));
    }

    #[test]
    fn serializes_addr_parts_flat() {
        let entity = Entity {
            name: "Midas".into(),
            osm_type: "node".into(),
            osm_id: 1,
            lat: Some(33.0),
            lon: Some(-112.0),
            address: "39 East Southern Avenue, Phoenix, AZ, 85040".into(),
            phone: String::new(),
            website: String::new(),
            operator: String::new(),
            tags: String::new(),
            address_parts: Entity::build_address_parts(&t(&[
                ("addr:housenumber", "39"),
                ("addr:street", "East Southern Avenue"),
                ("addr:city", "Phoenix"),
                ("addr:state", "AZ"),
                ("addr:postcode", "85040"),
            ])),
        };
        let v: serde_json::Value = serde_json::to_value(&entity).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.get("addr_city").and_then(|x| x.as_str()),
            Some("Phoenix")
        );
        assert_eq!(obj.get("addr_state").and_then(|x| x.as_str()), Some("AZ"));
        assert_eq!(
            obj.get("addr_housenumber").and_then(|x| x.as_str()),
            Some("39"),
        );
        // Joined form still present for back-compat.
        assert!(obj.get("address").is_some());
    }
}
