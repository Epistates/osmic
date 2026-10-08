use geo_types::{Coord, Geometry as GeoGeometry, LineString, MultiPolygon, Point, Polygon};
use mlt_core::v01::{PropValue, StagedLayer01, TileFeature as MltTileFeature, TileLayer01};
use mlt_core::EncodedLayer;
use osmic_osm::tags::{TagStore, WellKnownKey};
#[cfg(feature = "native")]
use pmtiles::TileType;

use crate::coord::TileTransform;
use crate::encode::{TileEncoder, TileFeature, TileFormat};

/// Property columns we encode into MLT tiles.
const PROPERTY_NAMES: &[&str] = &[
    "class",
    "name",
    "addr:street",
    "addr:housenumber",
    "addr:city",
    "addr:postcode",
    "phone",
    "website",
    "opening_hours",
    "cuisine",
    "brand",
    "operator",
];

/// Well-known keys corresponding to PROPERTY_NAMES (skipping "class" and "name"
/// which are handled separately).
const EXTRA_WK_KEYS: &[WellKnownKey] = &[
    WellKnownKey::AddrStreet,
    WellKnownKey::AddrHousenumber,
    WellKnownKey::AddrCity,
    WellKnownKey::AddrPostcode,
    WellKnownKey::Phone,
    WellKnownKey::Website,
    WellKnownKey::OpeningHours,
    WellKnownKey::Cuisine,
    WellKnownKey::Brand,
    WellKnownKey::Operator,
];

/// MLT (MapLibre Tile) encoder.
pub struct MltEncoder;

impl TileEncoder for MltEncoder {
    fn encode_clipped(
        &self,
        extent: u32,
        transform: &TileTransform,
        layer_features: &[(&str, Vec<&dyn TileFeature>)],
        tag_store: &TagStore,
    ) -> Option<Vec<u8>> {
        encode_mlt_tile(extent, Some(transform), layer_features, tag_store)
    }

    fn format(&self) -> TileFormat {
        TileFormat::Mlt
    }

    #[cfg(feature = "native")]
    fn tile_type(&self) -> TileType {
        // PMTiles uses Unknown(0x03) for MLT; check if pmtiles crate has Mlt variant
        // For now use Mvt as the container format — the actual encoding is MLT
        // TODO: Update when pmtiles crate adds TileType::Mlt
        TileType::Mvt
    }
}

fn encode_mlt_tile(
    extent: u32,
    transform: Option<&TileTransform>,
    layer_features: &[(&str, Vec<&dyn TileFeature>)],
    tag_store: &TagStore,
) -> Option<Vec<u8>> {
    let name_key = tag_store.well_known(WellKnownKey::Name);
    let extra_keys: Vec<_> = EXTRA_WK_KEYS
        .iter()
        .map(|wk| tag_store.well_known(*wk))
        .collect();

    let mut output = Vec::new();
    let mut any_layer = false;

    for &(layer_name, ref features) in layer_features {
        if features.is_empty() {
            continue;
        }

        let property_names: Vec<String> = PROPERTY_NAMES.iter().map(|s| s.to_string()).collect();
        let prop_count = property_names.len();

        let mlt_features: Vec<MltTileFeature> = features
            .iter()
            .filter_map(|&feature| {
                let geom = convert_geometry(feature.geometry(), extent, transform)?;

                let mut props = Vec::with_capacity(prop_count);

                // "class"
                props.push(PropValue::Str(Some(
                    feature.kind().class_name().to_string(),
                )));

                // "name"
                let name_val = feature
                    .tags()
                    .get(name_key)
                    .map(|v| tag_store.resolve(v).to_string());
                props.push(PropValue::Str(name_val));

                // Extra tag columns
                for &key in &extra_keys {
                    let val = feature
                        .tags()
                        .get(key)
                        .map(|v| tag_store.resolve(v).to_string());
                    props.push(PropValue::Str(val));
                }

                Some(MltTileFeature {
                    id: Some(feature.id() as u64),
                    geometry: geom,
                    properties: props,
                })
            })
            .collect();

        if mlt_features.is_empty() {
            continue;
        }

        let tile_layer = TileLayer01 {
            name: layer_name.to_string(),
            extent,
            property_names,
            features: mlt_features,
        };

        let staged = StagedLayer01::from(tile_layer);

        let encoded = match staged.encode_auto() {
            Ok((enc, _encoder)) => enc,
            Err(e) => {
                tracing::warn!(layer = layer_name, error = %e, "MLT encoding failed, skipping layer");
                continue;
            }
        };

        let layer = EncodedLayer::Tag01(encoded);
        if layer.write_to(&mut output).is_err() {
            continue;
        }
        any_layer = true;
    }

    if any_layer {
        Some(output)
    } else {
        None
    }
}

/// Convert osmic Geometry (f64 lon/lat or projected) to geo_types::Geometry<i32>
/// in tile-local coordinates suitable for MLT encoding.
fn convert_geometry(
    geom: &osmic_core::geometry::Geometry,
    extent: u32,
    transform: Option<&TileTransform>,
) -> Option<GeoGeometry<i32>> {
    match geom {
        osmic_core::geometry::Geometry::Point(p) => {
            let (x, y) = project(p.x(), p.y(), extent, transform);
            Some(GeoGeometry::Point(Point::new(x, y)))
        }
        osmic_core::geometry::Geometry::Line(ls) => {
            let coords: Vec<Coord<i32>> = ls
                .coords()
                .map(|c| {
                    let (x, y) = project(c.x, c.y, extent, transform);
                    Coord { x, y }
                })
                .collect();
            if coords.len() < 2 {
                return None;
            }
            Some(GeoGeometry::LineString(LineString::new(coords)))
        }
        osmic_core::geometry::Geometry::Polygon(poly) => {
            let exterior = convert_ring(poly.exterior(), extent, transform);
            let interiors: Vec<_> = poly
                .interiors()
                .iter()
                .map(|ring| convert_ring(ring, extent, transform))
                .collect();
            Some(GeoGeometry::Polygon(Polygon::new(exterior, interiors)))
        }
        osmic_core::geometry::Geometry::MultiPolygon(mp) => {
            let polys: Vec<_> = mp
                .iter()
                .map(|poly| {
                    let exterior = convert_ring(poly.exterior(), extent, transform);
                    let interiors: Vec<_> = poly
                        .interiors()
                        .iter()
                        .map(|ring| convert_ring(ring, extent, transform))
                        .collect();
                    Polygon::new(exterior, interiors)
                })
                .collect();
            Some(GeoGeometry::MultiPolygon(MultiPolygon::new(polys)))
        }
    }
}

fn convert_ring(
    ring: &geo_types::LineString<f64>,
    extent: u32,
    transform: Option<&TileTransform>,
) -> LineString<i32> {
    let coords: Vec<Coord<i32>> = ring
        .coords()
        .map(|c| {
            let (x, y) = project(c.x, c.y, extent, transform);
            Coord { x, y }
        })
        .collect();
    LineString::new(coords)
}

/// Project coordinates to tile-local i32 space.
/// If transform is Some, coords are lon/lat and need projection.
/// If None, coords are already in tile-local f64 space (GPU path).
fn project(x: f64, y: f64, _extent: u32, transform: Option<&TileTransform>) -> (i32, i32) {
    if let Some(t) = transform {
        let (tx, ty) = t.lon_lat_to_tile(x, y);
        (tx.round() as i32, ty.round() as i32)
    } else {
        (x.round() as i32, y.round() as i32)
    }
}
