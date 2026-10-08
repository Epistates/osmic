use std::io::Read;
use std::path::Path;
use std::time::Instant;

use geo_types::{Coord, LineString, MultiPolygon, Point, Polygon};
use tracing::info;

use osmic_core::bbox::BBox;
use osmic_core::error::{OsmicError, OsmicResult};
use osmic_core::geometry::Geometry;

use crate::classify;
use crate::feature::{Feature, FeatureKind};
use crate::pipeline::PipelineStats;
use crate::tags::{TagStore, Tags};

use std::sync::Arc;
use std::time::Duration;

/// Load features from a GeoJSON file.
///
/// Supports both FeatureCollection and individual Feature objects.
/// Properties are mapped to OSM-style tags for classification.
pub fn load_geojson(path: &Path) -> OsmicResult<crate::pipeline::ProcessedData> {
    let start = Instant::now();
    info!(path = %path.display(), "Loading GeoJSON");

    let mut file = std::fs::File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let json: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| OsmicError::Other(format!("Invalid JSON: {e}")))?;

    let tag_store = Arc::new(TagStore::new());
    let mut features = Vec::new();
    let mut bbox = BBox::empty();
    let mut id_counter: i64 = 1;

    let geojson_features = match json.get("type").and_then(|t| t.as_str()) {
        Some("FeatureCollection") => json
            .get("features")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default(),
        Some("Feature") => vec![json.clone()],
        _ => {
            return Err(OsmicError::Other(
                "Expected GeoJSON FeatureCollection or Feature".into(),
            ));
        }
    };

    for gj_feature in &geojson_features {
        let properties = gj_feature.get("properties").and_then(|p| p.as_object());
        let geometry_json = match gj_feature.get("geometry") {
            Some(g) => g,
            None => continue,
        };

        // Build tags from properties
        let mut tags = Tags::new();
        if let Some(props) = properties {
            for (key, value) in props {
                let v = match value {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                tags.push(tag_store.intern_key(key), tag_store.intern_value(&v));
            }
        }

        // Get feature ID
        let id = gj_feature
            .get("id")
            .and_then(|id| id.as_i64())
            .unwrap_or_else(|| {
                let i = id_counter;
                id_counter += 1;
                i
            });

        // Parse geometry
        let geometry = match parse_geojson_geometry(geometry_json, &mut bbox) {
            Some(g) => g,
            None => continue,
        };

        // Classify
        let kind = classify::classify(&tags, &tag_store, &crate::layers::LayerSet::all())
            .unwrap_or(FeatureKind::Natural(crate::feature::NaturalKind::Other));

        features.push(Feature {
            id,
            kind,
            geometry,
            tags,
        });
    }

    let duration = start.elapsed();
    info!(
        features = features.len(),
        elapsed_s = duration.as_secs_f64(),
        "GeoJSON loaded"
    );

    let feature_count = features.len() as u64;
    Ok(crate::pipeline::ProcessedData {
        tag_store,
        features,
        bbox,
        stats: PipelineStats {
            node_count: 0,
            way_count: 0,
            relation_count: 0,
            feature_count,
            pass1_duration: Duration::ZERO,
            pass2_duration: duration,
            total_duration: duration,
        },
    })
}

fn parse_geojson_geometry(json: &serde_json::Value, bbox: &mut BBox) -> Option<Geometry> {
    let geom_type = json.get("type")?.as_str()?;
    let coordinates = json.get("coordinates")?;

    match geom_type {
        "Point" => {
            let coords = coordinates.as_array()?;
            let lon = coords.first()?.as_f64()?;
            let lat = coords.get(1)?.as_f64()?;
            bbox.expand(lon, lat);
            Some(Geometry::Point(Point::new(lon, lat)))
        }
        "LineString" => {
            let coords = parse_coord_array(coordinates, bbox)?;
            if coords.len() >= 2 {
                Some(Geometry::Line(LineString::new(coords)))
            } else {
                None
            }
        }
        "Polygon" => {
            let rings = coordinates.as_array()?;
            let exterior = parse_coord_array(rings.first()?, bbox)?;
            if exterior.len() < 3 {
                return None;
            }
            let holes: Vec<LineString<f64>> = rings
                .iter()
                .skip(1)
                .filter_map(|r| parse_coord_array(r, bbox).map(LineString::new))
                .filter(|ls| ls.0.len() >= 3)
                .collect();
            Some(Geometry::Polygon(Polygon::new(
                LineString::new(exterior),
                holes,
            )))
        }
        "MultiPolygon" => {
            let polys_json = coordinates.as_array()?;
            let mut polys = Vec::new();
            for poly_json in polys_json {
                let rings = poly_json.as_array()?;
                let exterior = parse_coord_array(rings.first()?, bbox)?;
                if exterior.len() < 3 {
                    continue;
                }
                let holes: Vec<LineString<f64>> = rings
                    .iter()
                    .skip(1)
                    .filter_map(|r| parse_coord_array(r, bbox).map(LineString::new))
                    .filter(|ls| ls.0.len() >= 3)
                    .collect();
                polys.push(Polygon::new(LineString::new(exterior), holes));
            }
            if polys.is_empty() {
                None
            } else {
                Some(Geometry::MultiPolygon(MultiPolygon::new(polys)))
            }
        }
        _ => None,
    }
}

fn parse_coord_array(json: &serde_json::Value, bbox: &mut BBox) -> Option<Vec<Coord<f64>>> {
    let array = json.as_array()?;
    let coords: Vec<Coord<f64>> = array
        .iter()
        .filter_map(|c| {
            let arr = c.as_array()?;
            let lon = arr.first()?.as_f64()?;
            let lat = arr.get(1)?.as_f64()?;
            bbox.expand(lon, lat);
            Some(Coord { x: lon, y: lat })
        })
        .collect();
    if coords.is_empty() {
        None
    } else {
        Some(coords)
    }
}
