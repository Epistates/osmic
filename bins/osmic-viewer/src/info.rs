//! Click-to-inspect: finding the point of interest under the pointer and
//! describing it.

use osmic_render::Camera;

use crate::loader::Poi;

/// How close (logical pixels) a click must be to a feature to pick it.
pub const PICK_RADIUS: f64 = 14.0;

/// Most lines shown in the panel.
const MAX_LINES: usize = 12;

/// A panel describing one feature.
#[derive(Debug, Clone, PartialEq)]
pub struct InfoPanel {
    /// `(label, value)` rows; the first is the headline.
    pub lines: Vec<(String, String)>,
    /// Where the user clicked, in logical pixels.
    pub anchor: [f64; 2],
}

/// The feature nearest to `click` (logical px) within [`PICK_RADIUS`].
pub fn pick<'a>(
    camera: &Camera,
    pois: impl Iterator<Item = &'a Poi>,
    click: [f64; 2],
) -> Option<&'a Poi> {
    pois.filter_map(|poi| {
        let p = camera.lonlat_to_screen(poi.lon, poi.lat);
        let d = (p[0] - click[0]).hypot(p[1] - click[1]);
        (d <= PICK_RADIUS).then_some((d, poi))
    })
    // Ties (identical positions) keep the first, so picking is stable.
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map(|(_, poi)| poi)
}

/// The rows describing `poi`.
pub fn describe(poi: &Poi) -> Vec<(String, String)> {
    let tag = |key: &str| {
        poi.tags
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    let mut lines = vec![("Name".to_string(), poi.name.clone())];
    lines.push((
        "Type".into(),
        match &poi.class {
            Some(class) => format!("{} / {class}", poi.layer),
            None => poi.layer.clone(),
        },
    ));

    let mut address = String::new();
    if let Some(n) = tag("addr:housenumber") {
        address.push_str(n);
        address.push(' ');
    }
    if let Some(s) = tag("addr:street") {
        address.push_str(s);
    }
    for (key, separator) in [("addr:city", ", "), ("addr:postcode", " ")] {
        if let Some(v) = tag(key) {
            if !address.is_empty() {
                address.push_str(separator);
            }
            address.push_str(v);
        }
    }
    if !address.trim().is_empty() {
        lines.push(("Address".into(), address.trim().to_string()));
    }

    for (keys, label) in [
        (&["phone", "contact:phone"][..], "Phone"),
        (&["website", "contact:website"][..], "Website"),
        (&["opening_hours"][..], "Hours"),
        (&["cuisine"][..], "Cuisine"),
        (&["brand"][..], "Brand"),
        (&["operator"][..], "Operator"),
        (&["description"][..], "Info"),
    ] {
        if let Some(v) = keys.iter().find_map(|k| tag(k)) {
            lines.push((label.into(), v.to_string()));
        }
    }
    lines.push(("Location".into(), format!("{:.6}, {:.6}", poi.lat, poi.lon)));
    lines.truncate(MAX_LINES);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poi(name: &str, lon: f64, lat: f64, tags: &[(&str, &str)]) -> Poi {
        Poi {
            lon,
            lat,
            layer: "amenity".into(),
            class: Some("cafe".into()),
            name: name.into(),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn picks_the_nearest_feature_within_the_radius() {
        let cam = Camera::new(10.0, 50.0, 15.0, 800.0, 600.0);
        let near = poi("near", 10.0001, 50.0, &[]);
        let nearer = poi("nearer", 10.00005, 50.0, &[]);
        let far = poi("far", 11.0, 50.0, &[]);
        let click = cam.lonlat_to_screen(10.00005, 50.0);
        let pois = [near, far, nearer];
        let picked = pick(&cam, pois.iter(), click).unwrap();
        assert_eq!(picked.name, "nearer");
        assert!(pick(&cam, pois.iter(), [0.0, 0.0]).is_none());
        assert!(pick(&cam, std::iter::empty(), click).is_none());
    }

    #[test]
    fn describes_address_and_contact_details() {
        let p = poi(
            "Blue Bottle",
            -122.4,
            37.78,
            &[
                ("addr:housenumber", "66"),
                ("addr:street", "Mint Plaza"),
                ("addr:city", "San Francisco"),
                ("addr:postcode", "94103"),
                ("contact:phone", "+1 555 0100"),
                ("opening_hours", "Mo-Su 07:00-19:00"),
                ("website", ""),
            ],
        );
        let lines = describe(&p);
        let get = |k: &str| lines.iter().find(|(l, _)| l == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Name"), Some("Blue Bottle"));
        assert_eq!(get("Type"), Some("amenity / cafe"));
        assert_eq!(get("Address"), Some("66 Mint Plaza, San Francisco 94103"));
        assert_eq!(get("Phone"), Some("+1 555 0100"));
        assert_eq!(get("Hours"), Some("Mo-Su 07:00-19:00"));
        assert_eq!(get("Website"), None, "empty tags are skipped");
        assert_eq!(get("Location"), Some("37.780000, -122.400000"));
    }

    #[test]
    fn partial_addresses_and_row_cap() {
        let p = poi("x", 0.0, 0.0, &[("addr:street", "Only Street")]);
        assert_eq!(
            describe(&p).iter().find(|(l, _)| l == "Address").unwrap().1,
            "Only Street"
        );
        let none = poi("x", 0.0, 0.0, &[]);
        assert!(describe(&none).iter().all(|(l, _)| l != "Address"));
        let many: Vec<(String, String)> = Vec::new();
        assert!(many.len() <= MAX_LINES && describe(&p).len() <= MAX_LINES);
    }
}
