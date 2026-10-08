//! MapLibre Tile (MLT) encoder, via `mlt-core`.
//!
//! Each layer's columns are the union of its features' attribute keys
//! (sorted, `class` first), so `--all-tags` output is preserved.

use std::collections::BTreeSet;

use geo_types::{
    Coord, Geometry as GeoGeometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point,
    Polygon,
};
use mlt_core::encoder::EncoderConfig;
use mlt_core::{MltError, PropKind, PropValue, TileLayer as MltLayer};

use crate::encode::{TileEncoder, TileFormat};
use crate::error::TileError;
use crate::model::{GeomType, TileFeature, TileLayer, ring_area2};

/// MapLibre Tile encoder.
#[derive(Debug, Clone, Copy, Default)]
pub struct MltEncoder;

fn ring(points: &[[i32; 2]]) -> LineString<i32> {
    LineString(points.iter().map(|&[x, y]| Coord { x, y }).collect())
}

fn geometry(f: &TileFeature) -> Option<GeoGeometry<i32>> {
    match f.geom_type {
        GeomType::Point => {
            let pts: Vec<Point<i32>> = f
                .parts
                .iter()
                .flatten()
                .map(|&[x, y]| Point::new(x, y))
                .collect();
            match pts.len() {
                0 => None,
                1 => Some(GeoGeometry::Point(pts[0])),
                _ => Some(GeoGeometry::MultiPoint(MultiPoint(pts))),
            }
        }
        GeomType::LineString => {
            let lines: Vec<LineString<i32>> = f
                .parts
                .iter()
                .filter(|p| p.len() >= 2)
                .map(|p| ring(p))
                .collect();
            match lines.len() {
                0 => None,
                1 => lines.into_iter().next().map(GeoGeometry::LineString),
                _ => Some(GeoGeometry::MultiLineString(MultiLineString(lines))),
            }
        }
        GeomType::Polygon => {
            let mut polys: Vec<Polygon<i32>> = Vec::new();
            for part in f.parts.iter().filter(|p| p.len() >= 3) {
                if ring_area2(part) > 0 || polys.is_empty() {
                    polys.push(Polygon::new(ring(part), vec![]));
                } else if let Some(last) = polys.last_mut() {
                    last.interiors_push(ring(part));
                }
            }
            match polys.len() {
                0 => None,
                1 => polys.into_iter().next().map(GeoGeometry::Polygon),
                _ => Some(GeoGeometry::MultiPolygon(MultiPolygon(polys))),
            }
        }
    }
}

impl TileEncoder for MltEncoder {
    fn format(&self) -> TileFormat {
        TileFormat::Mlt
    }

    fn encode(&self, layers: &[TileLayer]) -> Result<Vec<u8>, TileError> {
        let mut out = Vec::new();
        for layer in layers.iter().filter(|l| !l.features.is_empty()) {
            let encode_err = |e: MltError| TileError::Encode {
                tile: format!("layer {}", layer.name),
                message: e.to_string(),
            };
            let mut keys: BTreeSet<&str> = layer
                .features
                .iter()
                .flat_map(|f| f.attributes.iter().map(|(k, _)| k.as_str()))
                .collect();
            keys.remove("class");
            let columns: Vec<&str> = std::iter::once("class").chain(keys).collect();

            let mut builder = MltLayer::builder(layer.name.as_str(), layer.extent)
                .map_err(encode_err)?;
            let column_keys = columns
                .iter()
                .map(|name| builder.add_property(*name, PropKind::Str))
                .collect::<Result<Vec<_>, _>>()
                .map_err(encode_err)?;
            for f in &layer.features {
                let Some(geometry) = geometry(f) else {
                    continue;
                };
                let mut row = builder.feature(geometry);
                row.id(f.id);
                for (k, v) in &f.attributes {
                    if let Some(i) = columns.iter().position(|c| c == k) {
                        row.property(column_keys[i], PropValue::Str(Some(v.clone())))
                            .map_err(encode_err)?;
                    }
                }
                row.finish().map_err(encode_err)?;
            }
            let layer = builder.finish();
            if layer.feature_count() == 0 {
                continue;
            }
            out.extend(layer.encode(EncoderConfig::default()).map_err(encode_err)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_every_geometry_type_with_dynamic_columns() {
        let f = |geom_type, parts: Vec<Vec<[i32; 2]>>, attrs: &[(&str, &str)]| TileFeature {
            id: Some(1),
            geom_type,
            parts,
            attributes: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let layer = TileLayer {
            name: "test".into(),
            extent: 4096,
            features: vec![
                f(
                    GeomType::Point,
                    vec![vec![[1, 2]]],
                    &[("class", "a"), ("name", "x")],
                ),
                f(
                    GeomType::LineString,
                    vec![vec![[0, 0], [5, 5]]],
                    &[("class", "b"), ("surface", "paved")],
                ),
                f(
                    GeomType::Polygon,
                    vec![
                        vec![[0, 0], [10, 0], [10, 10], [0, 10]],
                        vec![[2, 2], [2, 4], [4, 4], [4, 2]],
                    ],
                    &[("class", "c")],
                ),
            ],
        };
        let bytes = MltEncoder.encode(&[layer]).expect("encodes");

        let mut parser = mlt_core::Parser::default();
        let mut decoder = mlt_core::Decoder::default();
        let layers = parser.parse_layers(&bytes).expect("parses");
        assert_eq!(layers.len(), 1);
        let decoded = layers
            .into_iter()
            .next()
            .and_then(|l| l.into_tile(&mut decoder).expect("decodes"))
            .expect("tag 0x01 layer");
        assert_eq!(decoded.name(), "test");
        assert_eq!(decoded.extent().get(), 4096);
        assert_eq!(decoded.property_names(), ["class", "name", "surface"]);
        assert_eq!(decoded.feature_count(), 3);
        let point = decoded
            .features()
            .iter()
            .find(|f| matches!(f.geometry(), GeoGeometry::Point(_)))
            .expect("point survives");
        assert_eq!(point.id(), Some(1));
        assert_eq!(
            point.properties(),
            [
                PropValue::Str(Some("a".into())),
                PropValue::Str(Some("x".into())),
                PropValue::Str(None),
            ]
        );
        let polygon = decoded
            .features()
            .iter()
            .find_map(|f| match f.geometry() {
                GeoGeometry::Polygon(p) => Some(p),
                _ => None,
            })
            .expect("polygon survives");
        assert_eq!(polygon.interiors().len(), 1);
    }
}
