//! MapLibre Tile (MLT) encoder, via `mlt-core`.
//!
//! Each layer's columns are the union of its features' attribute keys
//! (sorted, `class` first), so `--all-tags` output is preserved.

use std::collections::BTreeSet;

use geo_types::{
    Coord, Geometry as GeoGeometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point,
    Polygon,
};
use mlt_core::EncodedLayer;
use mlt_core::v01::{PropValue, StagedLayer01, TileFeature as MltFeature, TileLayer01};

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
            let mut keys: BTreeSet<&str> = layer
                .features
                .iter()
                .flat_map(|f| f.attributes.iter().map(|(k, _)| k.as_str()))
                .collect();
            keys.remove("class");
            let property_names: Vec<String> = std::iter::once("class")
                .chain(keys)
                .map(str::to_string)
                .collect();
            let features: Vec<MltFeature> = layer
                .features
                .iter()
                .filter_map(|f| {
                    let geometry = geometry(f)?;
                    let properties = property_names
                        .iter()
                        .map(|name| {
                            PropValue::Str(
                                f.attributes
                                    .iter()
                                    .find(|(k, _)| k == name)
                                    .map(|(_, v)| v.clone()),
                            )
                        })
                        .collect();
                    Some(MltFeature {
                        id: f.id,
                        geometry,
                        properties,
                    })
                })
                .collect();
            if features.is_empty() {
                continue;
            }
            let staged = StagedLayer01::from(TileLayer01 {
                name: layer.name.clone(),
                extent: layer.extent,
                property_names,
                features,
            });
            let encode_err = |message: String| TileError::Encode {
                tile: format!("layer {}", layer.name),
                message,
            };
            let (encoded, _) = staged
                .encode_auto()
                .map_err(|e| encode_err(e.to_string()))?;
            EncodedLayer::Tag01(encoded)
                .write_to(&mut out)
                .map_err(|e| encode_err(e.to_string()))?;
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
        assert!(!bytes.is_empty());
    }
}
