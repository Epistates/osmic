use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// MapLibre-compatible style definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Style {
    pub version: u8,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glyphs: Option<String>,
    pub sources: Value,
    pub layers: Vec<Value>,
}

impl Style {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// Generate a default MapLibre GL JS style for osmic tiles.
///
/// `source_url` should be the path/URL to the PMTiles archive,
/// e.g., "pmtiles://tiles.pmtiles" or "http://localhost:3000/{z}/{x}/{y}.mvt"
pub fn default_style_json(source_url: &str) -> Style {
    let is_pmtiles = source_url.starts_with("pmtiles://") || source_url.ends_with(".pmtiles");

    let source = if is_pmtiles {
        json!({
            "osmic": {
                "type": "vector",
                "url": source_url
            }
        })
    } else {
        json!({
            "osmic": {
                "type": "vector",
                "tiles": [source_url]
            }
        })
    };

    let layers = vec![
        // Background
        json!({
            "id": "background",
            "type": "background",
            "paint": { "background-color": "#f8f4f0" }
        }),
        // Landuse
        json!({
            "id": "landuse-fill",
            "type": "fill",
            "source": "osmic",
            "source-layer": "landuse",
            "minzoom": 7,
            "paint": {
                "fill-color": [
                    "match", ["get", "class"],
                    "forest", "#add19e",
                    "grass", "#cdebb0",
                    "meadow", "#cdebb0",
                    "farmland", "#d5e29e",
                    "residential", "#e0d6d0",
                    "commercial", "#f2dad9",
                    "industrial", "#ebdbe8",
                    "cemetery", "#aacbaf",
                    "#d5cfc8"
                ],
                "fill-opacity": 0.8
            }
        }),
        // Natural areas
        json!({
            "id": "natural-fill",
            "type": "fill",
            "source": "osmic",
            "source-layer": "natural",
            "minzoom": 6,
            "paint": {
                "fill-color": [
                    "match", ["get", "class"],
                    "wood", "#add19e",
                    "scrub", "#c8d7ab",
                    "grassland", "#cdebb0",
                    "sand", "#f5e9c6",
                    "beach", "#fff1ba",
                    "glacier", "#ddecec",
                    "#e8e0d8"
                ],
                "fill-opacity": 0.6
            }
        }),
        // Leisure areas
        json!({
            "id": "leisure-fill",
            "type": "fill",
            "source": "osmic",
            "source-layer": "leisure",
            "minzoom": 8,
            "paint": {
                "fill-color": [
                    "match", ["get", "class"],
                    "park", "#c8facc",
                    "garden", "#cdebb0",
                    "golf_course", "#b5e3b5",
                    "nature_reserve", "#cdebb0",
                    "#c8facc"
                ],
                "fill-opacity": 0.6
            }
        }),
        // Water fill
        json!({
            "id": "water-fill",
            "type": "fill",
            "source": "osmic",
            "source-layer": "water",
            "filter": ["in", "class", "lake", "pond", "reservoir", "basin"],
            "paint": {
                "fill-color": "#aad3df",
                "fill-opacity": 0.8
            }
        }),
        // Water lines
        json!({
            "id": "water-line",
            "type": "line",
            "source": "osmic",
            "source-layer": "water",
            "filter": ["in", "class", "river", "stream", "canal", "drain", "ditch"],
            "paint": {
                "line-color": "#aad3df",
                "line-width": [
                    "match", ["get", "class"],
                    "river", 3,
                    "canal", 2,
                    1
                ]
            }
        }),
        // Building fill
        json!({
            "id": "building-fill",
            "type": "fill",
            "source": "osmic",
            "source-layer": "building",
            "minzoom": 13,
            "paint": {
                "fill-color": "#dfdbd7",
                "fill-opacity": 0.8
            }
        }),
        // Building outline
        json!({
            "id": "building-outline",
            "type": "line",
            "source": "osmic",
            "source-layer": "building",
            "minzoom": 14,
            "paint": {
                "line-color": "#c9c0b8",
                "line-width": 0.5
            }
        }),
        // Boundary
        json!({
            "id": "boundary",
            "type": "line",
            "source": "osmic",
            "source-layer": "boundary",
            "minzoom": 2,
            "paint": {
                "line-color": "#9e9cab",
                "line-width": 1.5,
                "line-dasharray": [4, 2]
            }
        }),
        // Railway
        json!({
            "id": "railway",
            "type": "line",
            "source": "osmic",
            "source-layer": "railway",
            "minzoom": 8,
            "paint": {
                "line-color": "#bfbfbf",
                "line-width": 1.0
            }
        }),
        // Highway casing (wider, darker line underneath)
        json!({
            "id": "highway-casing",
            "type": "line",
            "source": "osmic",
            "source-layer": "highway",
            "minzoom": 7,
            "layout": { "line-cap": "round", "line-join": "round" },
            "paint": {
                "line-color": "#c0b8b0",
                "line-width": [
                    "match", ["get", "class"],
                    "motorway", 8,
                    "trunk", 7,
                    "primary", 6,
                    "secondary", 5,
                    "tertiary", 4,
                    "residential", 3,
                    "service", 2,
                    1.5
                ]
            }
        }),
        // Highway fill
        json!({
            "id": "highway-fill",
            "type": "line",
            "source": "osmic",
            "source-layer": "highway",
            "minzoom": 4,
            "layout": { "line-cap": "round", "line-join": "round" },
            "paint": {
                "line-color": [
                    "match", ["get", "class"],
                    "motorway", "#e892a2",
                    "motorway_link", "#e892a2",
                    "trunk", "#f9b29c",
                    "trunk_link", "#f9b29c",
                    "primary", "#fcd6a4",
                    "primary_link", "#fcd6a4",
                    "secondary", "#f7fabf",
                    "secondary_link", "#f7fabf",
                    "tertiary", "#ffffff",
                    "tertiary_link", "#ffffff",
                    "#ffffff"
                ],
                "line-width": [
                    "match", ["get", "class"],
                    "motorway", 6,
                    "trunk", 5,
                    "primary", 4,
                    "secondary", 3,
                    "tertiary", 2.5,
                    "residential", 1.5,
                    "service", 1,
                    0.75
                ]
            }
        }),
        // Road labels (along the line)
        json!({
            "id": "highway-label-major",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "highway",
            "minzoom": 10,
            "filter": ["in", "class", "motorway", "trunk", "primary", "secondary"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": [
                    "match", ["get", "class"],
                    "motorway", 13,
                    "trunk", 12,
                    "primary", 11,
                    10
                ],
                "symbol-placement": "line",
                "text-rotation-alignment": "map",
                "text-max-angle": 30,
                "text-padding": 20
            },
            "paint": {
                "text-color": "#555",
                "text-halo-color": "#fff",
                "text-halo-width": 1.5
            }
        }),
        json!({
            "id": "highway-label-minor",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "highway",
            "minzoom": 14,
            "filter": ["in", "class", "tertiary", "residential", "unclassified", "service", "living_street"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "symbol-placement": "line",
                "text-rotation-alignment": "map",
                "text-max-angle": 30,
                "text-padding": 10
            },
            "paint": {
                "text-color": "#666",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Water labels
        json!({
            "id": "water-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "water",
            "minzoom": 10,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 12,
                "symbol-placement": "line",
                "text-rotation-alignment": "map",
                "text-max-angle": 30,
                "text-padding": 30
            },
            "paint": {
                "text-color": "#6b9daf",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Natural/leisure area labels
        json!({
            "id": "area-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "leisure",
            "minzoom": 12,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 11,
                "text-padding": 10
            },
            "paint": {
                "text-color": "#3a7a3a",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Amenity/POI labels
        json!({
            "id": "amenity-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "amenity",
            "minzoom": 15,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "text-padding": 5,
                "icon-allow-overlap": false,
                "text-allow-overlap": false
            },
            "paint": {
                "text-color": "#734a08",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Shop labels
        json!({
            "id": "shop-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "shop",
            "minzoom": 15,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "text-padding": 5,
                "text-allow-overlap": false
            },
            "paint": {
                "text-color": "#5b3a0a",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Tourism labels
        json!({
            "id": "tourism-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "tourism",
            "minzoom": 14,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "text-padding": 5,
                "text-allow-overlap": false
            },
            "paint": {
                "text-color": "#0d7377",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Healthcare labels
        json!({
            "id": "healthcare-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "healthcare",
            "minzoom": 15,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "text-padding": 5,
                "text-allow-overlap": false
            },
            "paint": {
                "text-color": "#c4281c",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Office labels
        json!({
            "id": "office-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "office",
            "minzoom": 15,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "text-padding": 5,
                "text-allow-overlap": false
            },
            "paint": {
                "text-color": "#555",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Craft labels
        json!({
            "id": "craft-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "craft",
            "minzoom": 15,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "text-padding": 5,
                "text-allow-overlap": false
            },
            "paint": {
                "text-color": "#b5651d",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Historic labels
        json!({
            "id": "historic-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "historic",
            "minzoom": 14,
            "filter": ["has", "name"],
            "layout": {
                "text-field": ["get", "name"],
                "text-font": ["Open Sans Regular"],
                "text-size": 10,
                "text-padding": 5,
                "text-allow-overlap": false
            },
            "paint": {
                "text-color": "#7b2d8b",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        }),
        // Place labels
        json!({
            "id": "place-label",
            "type": "symbol",
            "source": "osmic",
            "source-layer": "place",
            "minzoom": 4,
            "layout": {
                "text-field": ["get", "name"],
                "text-size": [
                    "match", ["get", "class"],
                    "city", 20,
                    "town", 15,
                    "village", 12,
                    10
                ],
                "text-font": ["Open Sans Regular"],
                "text-padding": 5
            },
            "paint": {
                "text-color": "#333",
                "text-halo-color": "#fff",
                "text-halo-width": 2
            }
        }),
    ];

    Style {
        version: 8,
        name: "Osmic Default".into(),
        glyphs: Some("https://fonts.openmaptiles.org/{fontstack}/{range}.pbf".into()),
        sources: source,
        layers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_style_pmtiles_source_uses_url_key() {
        let style = default_style_json("pmtiles://tiles.pmtiles");
        assert_eq!(style.version, 8);
        assert_eq!(style.name, "Osmic Default");
        let osmic_source = &style.sources["osmic"];
        assert_eq!(osmic_source["type"], "vector");
        assert_eq!(osmic_source["url"], "pmtiles://tiles.pmtiles");
        assert!(osmic_source.get("tiles").is_none());
    }

    #[test]
    fn default_style_http_source_uses_tiles_array() {
        let url = "http://localhost:3000/{z}/{x}/{y}.mvt";
        let style = default_style_json(url);
        let osmic_source = &style.sources["osmic"];
        assert_eq!(osmic_source["type"], "vector");
        assert_eq!(osmic_source["tiles"][0], url);
        assert!(osmic_source.get("url").is_none());
    }

    #[test]
    fn default_style_has_background_and_nonempty_layers() {
        let style = default_style_json("pmtiles://x.pmtiles");
        assert!(!style.layers.is_empty());
        assert_eq!(style.layers[0]["id"], "background");
        assert_eq!(style.layers[0]["type"], "background");
    }

    #[test]
    fn style_roundtrips_through_json() {
        let style = default_style_json("pmtiles://x.pmtiles");
        let json = style.to_json();
        assert!(!json.is_empty());
        let parsed: Style = serde_json::from_str(&json).expect("roundtrip");
        assert_eq!(parsed.version, style.version);
        assert_eq!(parsed.name, style.name);
        assert_eq!(parsed.layers.len(), style.layers.len());
    }
}
