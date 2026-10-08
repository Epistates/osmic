//! GeoJSON input.
//!
//! The document is parsed as a stream — the raw JSON is never held in
//! memory — and the resulting features are collected (like
//! [`PbfProcessor::process`](crate::PbfProcessor::process)). Properties
//! become tags and are classified like OSM tags; features that match no
//! enabled layer are counted and skipped.

use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use geo_types::{Coord, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use tracing::{info, warn};

use osmic_core::{BBox, Geometry, OsmId, OsmType};

use crate::classify::{KeyValues, classify};
use crate::error::OsmError;
use crate::feature::Feature;
use crate::layers::LayerSet;
use crate::pbf::PbfHeader;
use crate::pipeline::{PipelineStats, ProcessedData};
use crate::tags::{TagRetention, TagStore};

#[derive(Deserialize)]
struct GjFeature {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    geometry: Option<GjGeometry>,
    #[serde(default)]
    properties: Option<serde_json::Map<String, serde_json::Value>>,
}

type Position = Vec<f64>;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum GjGeometry {
    Point {
        coordinates: Position,
    },
    MultiPoint {
        coordinates: Vec<Position>,
    },
    LineString {
        coordinates: Vec<Position>,
    },
    MultiLineString {
        coordinates: Vec<Vec<Position>>,
    },
    Polygon {
        coordinates: Vec<Vec<Position>>,
    },
    MultiPolygon {
        coordinates: Vec<Vec<Vec<Position>>>,
    },
    GeometryCollection {},
}

fn coord(p: &[f64]) -> Option<Coord<f64>> {
    let (&x, &y) = (p.first()?, p.get(1)?);
    let ok = x.is_finite()
        && y.is_finite()
        && (-180.0..=180.0).contains(&x)
        && (-90.0..=90.0).contains(&y);
    ok.then_some(Coord { x, y })
}

fn line(ps: &[Position]) -> Option<LineString<f64>> {
    let cs: Option<Vec<_>> = ps.iter().map(|p| coord(p)).collect();
    cs.filter(|c| c.len() >= 2).map(LineString)
}

fn polygon(rings: &[Vec<Position>]) -> Option<Polygon<f64>> {
    let mut rings = rings.iter().map(|r| line(r).filter(|l| l.0.len() >= 4));
    let exterior = rings.next()??;
    Some(Polygon::new(exterior, rings.flatten().collect()))
}

impl GjGeometry {
    fn to_geometry(&self) -> Option<Geometry> {
        let mut g = match self {
            Self::Point { coordinates } => Geometry::Point(Point(coord(coordinates)?)),
            Self::MultiPoint { coordinates } => Geometry::MultiPoint(MultiPoint(
                coordinates
                    .iter()
                    .map(|p| coord(p).map(Point))
                    .collect::<Option<_>>()?,
            )),
            Self::LineString { coordinates } => Geometry::Line(line(coordinates)?),
            Self::MultiLineString { coordinates } => Geometry::MultiLine(MultiLineString(
                coordinates.iter().map(|l| line(l)).collect::<Option<_>>()?,
            )),
            Self::Polygon { coordinates } => Geometry::Polygon(polygon(coordinates)?),
            Self::MultiPolygon { coordinates } => Geometry::MultiPolygon(MultiPolygon(
                coordinates
                    .iter()
                    .map(|p| polygon(p))
                    .collect::<Option<_>>()?,
            )),
            Self::GeometryCollection {} => return None,
        };
        osmic_geo::orient_geometry(&mut g);
        Some(g)
    }
}

/// Parse `node/123`, `way/123`, `relation/123` (osmtogeojson) or
/// `n123`/`w123`/`r123` (osmium).
fn parse_osm_id(s: &str) -> Option<OsmId> {
    let (ty, num) = if let Some((t, n)) = s.split_once('/') {
        let ty = match t {
            "node" => OsmType::Node,
            "way" => OsmType::Way,
            "relation" => OsmType::Relation,
            _ => return None,
        };
        (ty, n)
    } else {
        let mut chars = s.chars();
        let ty = match chars.next()? {
            'n' => OsmType::Node,
            'w' => OsmType::Way,
            'r' => OsmType::Relation,
            _ => return None,
        };
        (ty, chars.as_str())
    };
    Some(OsmId::new(ty, num.parse().ok()?))
}

struct Loader<'a> {
    tag_store: &'a TagStore,
    layers: LayerSet,
    retention: TagRetention,
    features: Vec<Feature>,
    bbox: BBox,
    next_id: i64,
    unclassified: u64,
    invalid: u64,
}

impl Loader<'_> {
    fn add(&mut self, f: GjFeature) {
        let props = f.properties.unwrap_or_default();
        let tags: Vec<(&str, String)> = props
            .iter()
            .filter_map(|(k, v)| {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    _ => return None,
                };
                Some((k.as_str(), s))
            })
            .collect();
        let borrowed = || tags.iter().map(|(k, v)| (*k, v.as_str()));

        let id =
            f.id.as_ref()
                .and_then(|v| v.as_str())
                .and_then(parse_osm_id)
                .or_else(|| {
                    props
                        .get("@id")
                        .and_then(|v| v.as_str())
                        .and_then(parse_osm_id)
                })
                .or_else(|| f.id.as_ref().and_then(|v| v.as_i64()).map(OsmId::node))
                .unwrap_or_else(|| {
                    // Synthetic ids are negative so they never collide with OSM.
                    self.next_id += 1;
                    OsmId::node(-self.next_id)
                });

        let Some(geometry) = f.geometry.as_ref().and_then(GjGeometry::to_geometry) else {
            self.invalid += 1;
            return;
        };
        let kv = KeyValues::scan(borrowed());
        let classes = classify(&kv, self.layers);
        if classes.is_empty() {
            self.unclassified += 1;
            return;
        }
        let interned = self.tag_store.intern_tags(borrowed(), &self.retention);
        self.bbox.extend(&geometry.bbox());
        for c in &classes {
            self.features.push(Feature {
                id,
                kind: c.kind,
                geometry: geometry.clone(),
                tags: interned.clone(),
            });
        }
    }
}

/// Streams the members of the top-level object, handling both a
/// `FeatureCollection` and a lone `Feature`.
struct TopLevel<'l, 'a>(&'l mut Loader<'a>);

struct FeaturesSeed<'l, 'a>(&'l mut Loader<'a>);

impl<'de> DeserializeSeed<'de> for FeaturesSeed<'_, '_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for FeaturesSeed<'_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of GeoJSON features")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(feature) = seq.next_element::<GjFeature>()? {
            self.0.add(feature);
        }
        Ok(())
    }
}

impl<'de> Visitor<'de> for TopLevel<'_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a GeoJSON FeatureCollection or Feature")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut kind: Option<String> = None;
        let mut single = GjFeature {
            id: None,
            geometry: None,
            properties: None,
        };
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => kind = Some(map.next_value()?),
                "features" => map.next_value_seed(FeaturesSeed(self.0))?,
                "geometry" => single.geometry = map.next_value()?,
                "properties" => single.properties = map.next_value()?,
                "id" => single.id = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        match kind.as_deref() {
            Some("FeatureCollection") => Ok(()),
            Some("Feature") => {
                self.0.add(single);
                Ok(())
            }
            other => Err(de::Error::custom(format!(
                "expected FeatureCollection or Feature, found {other:?}"
            ))),
        }
    }
}

/// Load a GeoJSON file, classifying properties like OSM tags into `layers`.
pub fn load_geojson(path: &Path, layers: LayerSet) -> Result<ProcessedData, OsmError> {
    load_geojson_with(path, layers, &TagRetention::All)
}

/// [`load_geojson`] keeping only the properties `retention` selects.
pub fn load_geojson_with(
    path: &Path,
    layers: LayerSet,
    retention: &TagRetention,
) -> Result<ProcessedData, OsmError> {
    let start = Instant::now();
    info!(path = %path.display(), "Loading GeoJSON");
    let tag_store = Arc::new(TagStore::new());
    let mut loader = Loader {
        tag_store: &tag_store,
        layers,
        retention: retention.clone(),
        features: Vec::new(),
        bbox: BBox::empty(),
        next_id: 0,
        unclassified: 0,
        invalid: 0,
    };
    let reader = BufReader::new(File::open(path)?);
    let mut de = serde_json::Deserializer::from_reader(reader);
    de.deserialize_map(TopLevel(&mut loader))
        .and_then(|()| de.end())
        .map_err(|e| OsmError::GeoJson(format!("{}: {e}", path.display())))?;

    if loader.unclassified > 0 || loader.invalid > 0 {
        warn!(
            unclassified = loader.unclassified,
            invalid_geometry = loader.invalid,
            "GeoJSON features skipped"
        );
    }
    let Loader {
        mut features, bbox, ..
    } = loader;
    features.sort_by_key(|f| (f.id, f.kind.layer()));
    let duration = start.elapsed();
    info!(
        features = features.len(),
        secs = duration.as_secs_f64(),
        "GeoJSON loaded"
    );
    Ok(ProcessedData {
        header: PbfHeader::default(),
        tag_store,
        stats: PipelineStats {
            feature_count: features.len() as u64,
            pass2_duration: duration,
            total_duration: duration,
            ..Default::default()
        },
        features,
        bbox,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(json: &str) -> Result<ProcessedData, OsmError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("in.geojson");
        std::fs::write(&path, json).expect("write");
        load_geojson(&path, LayerSet::all())
    }

    #[test]
    fn feature_collection_streams_and_classifies() {
        let data = load(
            r#"{"features": [
                {"type":"Feature","id":"way/7","properties":{"highway":"primary","name":"Main"},
                 "geometry":{"type":"LineString","coordinates":[[0,0],[1,1,30]]}},
                {"type":"Feature","properties":{"building":"yes","amenity":"school"},
                 "geometry":{"type":"Polygon","coordinates":[[[0,0],[0,1],[1,1],[1,0],[0,0]]]}},
                {"type":"Feature","properties":{"foo":"bar"},
                 "geometry":{"type":"Point","coordinates":[5,5]}},
                {"type":"Feature","properties":{"shop":"bakery"},
                 "geometry":{"type":"Point","coordinates":[500,5]}}
            ], "type": "FeatureCollection"}"#,
        )
        .expect("valid");
        // highway + (amenity, building); the unclassified and invalid ones skipped.
        assert_eq!(data.features.len(), 3);
        assert!(data.features.iter().any(|f| f.id == OsmId::way(7)));
        let poly = data
            .features
            .iter()
            .find_map(|f| match &f.geometry {
                Geometry::Polygon(p) => Some(p),
                _ => None,
            })
            .expect("polygon");
        use geo::Winding;
        assert!(poly.exterior().is_ccw(), "polygons are oriented on load");
    }

    #[test]
    fn single_feature_document() {
        let data = load(
            r#"{"type":"Feature","properties":{"amenity":"cafe","@id":"node/42"},
                "geometry":{"type":"Point","coordinates":[1,2]}}"#,
        )
        .expect("valid");
        assert_eq!(data.features.len(), 1);
        assert_eq!(data.features[0].id, OsmId::node(42));
    }

    #[test]
    fn rejects_non_geojson() {
        assert!(matches!(
            load(r#"{"type":"Topology"}"#),
            Err(OsmError::GeoJson(_))
        ));
        assert!(load("[1,2,3]").is_err());
        assert!(load("{").is_err());
    }

    #[test]
    fn osm_id_parsing() {
        assert_eq!(parse_osm_id("relation/5"), Some(OsmId::relation(5)));
        assert_eq!(parse_osm_id("w12"), Some(OsmId::way(12)));
        assert_eq!(parse_osm_id("x/1"), None);
        assert_eq!(parse_osm_id("node/abc"), None);
    }
}
