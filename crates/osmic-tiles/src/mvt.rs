use mvt::{GeomData, GeomEncoder, GeomType, Tile};
use osmic_core::geometry::Geometry;
use osmic_osm::tags::{TagStore, WellKnownKey};
#[cfg(feature = "native")]
use pmtiles::TileType;

use crate::coord::TileTransform;
use crate::encode::{TileEncoder, TileFeature, TileFormat};

/// Encode a geometry into MVT GeomData using the given tile transform.
pub fn encode_geometry(
    geometry: &Geometry,
    transform: &TileTransform,
) -> Result<GeomData, mvt::Error> {
    match geometry {
        Geometry::Point(p) => {
            let (x, y) = transform.lon_lat_to_tile(p.x(), p.y());
            GeomEncoder::new(GeomType::Point).point(x, y)?.encode()
        }
        Geometry::Line(ls) => {
            let mut encoder = GeomEncoder::new(GeomType::Linestring);
            for coord in ls.coords() {
                let (x, y) = transform.lon_lat_to_tile(coord.x, coord.y);
                encoder.add_point(x, y)?;
            }
            encoder.encode()
        }
        Geometry::Polygon(poly) => {
            let mut encoder = GeomEncoder::new(GeomType::Polygon);
            let rings: Vec<_> = std::iter::once(poly.exterior())
                .chain(poly.interiors())
                .collect();
            for (i, ring) in rings.iter().enumerate() {
                for coord in ring.coords() {
                    let (x, y) = transform.lon_lat_to_tile(coord.x, coord.y);
                    encoder.add_point(x, y)?;
                }
                if i < rings.len() - 1 {
                    encoder.complete_geom()?;
                }
            }
            encoder.encode()
        }
        Geometry::MultiPolygon(mp) => {
            let mut encoder = GeomEncoder::new(GeomType::Polygon);
            let total_polys = mp.0.len();
            for (pi, poly) in mp.iter().enumerate() {
                let rings: Vec<_> = std::iter::once(poly.exterior())
                    .chain(poly.interiors())
                    .collect();
                for (ri, ring) in rings.iter().enumerate() {
                    for coord in ring.coords() {
                        let (x, y) = transform.lon_lat_to_tile(coord.x, coord.y);
                        encoder.add_point(x, y)?;
                    }
                    let is_last = pi == total_polys - 1 && ri == rings.len() - 1;
                    if !is_last {
                        encoder.complete_geom()?;
                    }
                }
            }
            encoder.encode()
        }
    }
}

/// Extra tag keys to encode for POI detail (address, contact, etc.)
const EXTRA_TAG_KEYS: &[(WellKnownKey, &str)] = &[
    (WellKnownKey::AddrStreet, "addr:street"),
    (WellKnownKey::AddrHousenumber, "addr:housenumber"),
    (WellKnownKey::AddrCity, "addr:city"),
    (WellKnownKey::AddrPostcode, "addr:postcode"),
    (WellKnownKey::Phone, "phone"),
    (WellKnownKey::ContactPhone, "contact:phone"),
    (WellKnownKey::Website, "website"),
    (WellKnownKey::ContactWebsite, "contact:website"),
    (WellKnownKey::OpeningHours, "opening_hours"),
    (WellKnownKey::Cuisine, "cuisine"),
    (WellKnownKey::Brand, "brand"),
    (WellKnownKey::Operator, "operator"),
    (WellKnownKey::Description, "description"),
];

fn encode_feature_tags(
    mvt_feature: &mut mvt::Feature,
    feature: &dyn TileFeature,
    tag_store: &TagStore,
    name_key: osmic_osm::tags::TagKey,
    extra_keys: &[(osmic_osm::tags::TagKey, &str)],
    include_all_tags: bool,
) {
    mvt_feature.add_tag_string("class", feature.kind().class_name());

    if include_all_tags {
        // Dump every tag on the feature, resolving interned keys/values
        // through the TagStore. Skip "class" since we just set it above,
        // and skip any key that produces an empty string.
        for (k, v) in feature.tags().iter() {
            let key_str = tag_store.resolve(*k);
            if key_str.is_empty() || key_str == "class" {
                continue;
            }
            let val_str = tag_store.resolve(*v);
            mvt_feature.add_tag_string(key_str, val_str);
        }
        return;
    }

    if let Some(name_val) = feature.tags().get(name_key) {
        mvt_feature.add_tag_string("name", tag_store.resolve(name_val));
    }

    for &(key, mvt_name) in extra_keys {
        if let Some(val) = feature.tags().get(key) {
            mvt_feature.add_tag_string(mvt_name, tag_store.resolve(val));
        }
    }
}

fn resolve_extra_keys(tag_store: &TagStore) -> Vec<(osmic_osm::tags::TagKey, &'static str)> {
    EXTRA_TAG_KEYS
        .iter()
        .map(|(wk, mvt_name)| (tag_store.well_known(*wk), *mvt_name))
        .collect()
}

/// Build an MVT tile from clipped features grouped by layer name.
pub fn build_tile_clipped(
    extent: u32,
    transform: &TileTransform,
    layer_features: &[(&str, Vec<&dyn TileFeature>)],
    tag_store: &TagStore,
    include_all_tags: bool,
) -> Option<Vec<u8>> {
    let mut tile = Tile::new(extent);
    let name_key = tag_store.well_known(WellKnownKey::Name);
    let extra_keys = resolve_extra_keys(tag_store);

    for &(layer_name, ref features) in layer_features {
        if features.is_empty() {
            continue;
        }

        let mut layer = tile.create_layer(layer_name);

        for &feature in features {
            match encode_geometry(feature.geometry(), transform) {
                Ok(geom_data) => {
                    let mut mvt_feature = layer.into_feature(geom_data);
                    mvt_feature.set_id(feature.id() as u64);
                    encode_feature_tags(
                        &mut mvt_feature,
                        feature,
                        tag_store,
                        name_key,
                        &extra_keys,
                        include_all_tags,
                    );
                    layer = mvt_feature.into_layer();
                }
                Err(_) => continue,
            }
        }

        if layer.num_features() > 0 {
            let _ = tile.add_layer(layer);
        }
    }

    if tile.num_layers() == 0 {
        return None;
    }

    tile.to_bytes().ok()
}

/// MVT tile encoder.
///
/// By default, only a curated whitelist of tag keys is written into each
/// MVT feature (see `EXTRA_TAG_KEYS`). Set `include_all_tags = true` to
/// emit every OSM tag present on the feature — useful for downstream
/// consumers that want to slice by arbitrary attributes, at the cost of
/// larger tiles (typically 3-5× on POI-dense areas).
#[derive(Debug, Clone, Copy, Default)]
pub struct MvtEncoder {
    pub include_all_tags: bool,
}

impl MvtEncoder {
    /// Create an encoder that writes the curated whitelist of tags only.
    pub fn new() -> Self {
        Self {
            include_all_tags: false,
        }
    }

    /// Create an encoder that emits every OSM tag on each feature.
    pub fn with_all_tags() -> Self {
        Self {
            include_all_tags: true,
        }
    }
}

impl TileEncoder for MvtEncoder {
    fn encode_clipped(
        &self,
        extent: u32,
        transform: &TileTransform,
        layer_features: &[(&str, Vec<&dyn TileFeature>)],
        tag_store: &TagStore,
    ) -> Option<Vec<u8>> {
        build_tile_clipped(
            extent,
            transform,
            layer_features,
            tag_store,
            self.include_all_tags,
        )
    }

    fn format(&self) -> TileFormat {
        TileFormat::Mvt
    }

    #[cfg(feature = "native")]
    fn tile_type(&self) -> TileType {
        TileType::Mvt
    }
}
