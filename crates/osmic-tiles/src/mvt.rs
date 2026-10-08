//! Mapbox Vector Tile 2.1 encoder.
//!
//! Encodes [`TileLayer`]s whose features are already clipped, quantised
//! and wound per the spec (see [`crate::model::TileFeature`]). Keys and
//! values are deduplicated per layer; geometry uses the standard
//! MoveTo/LineTo/ClosePath command stream with the cursor carried across
//! parts, and rings are written without their closing vertex.

use std::collections::HashMap;

use crate::model::{GeomType, TileFeature, TileLayer};
use crate::proto::{
    put_bytes_field, put_message, put_packed_varints, put_varint_field, zigzag_encode32,
};

const CMD_MOVE_TO: u32 = 1;
const CMD_LINE_TO: u32 = 2;
const CMD_CLOSE_PATH: u32 = 7;

fn command(id: u32, count: usize) -> u64 {
    u64::from((id & 0x7) | ((count as u32) << 3))
}

/// Append the MVT geometry command stream for `feature` to `out`.
pub fn encode_geometry(feature: &TileFeature, out: &mut Vec<u64>) {
    let (mut cx, mut cy) = (0i32, 0i32);
    let mut point = |out: &mut Vec<u64>, [x, y]: [i32; 2]| {
        out.push(u64::from(zigzag_encode32(x.wrapping_sub(cx))));
        out.push(u64::from(zigzag_encode32(y.wrapping_sub(cy))));
        (cx, cy) = (x, y);
    };
    match feature.geom_type {
        GeomType::Point => {
            let count: usize = feature.parts.iter().map(Vec::len).sum();
            if count == 0 {
                return;
            }
            out.push(command(CMD_MOVE_TO, count));
            for &p in feature.parts.iter().flatten() {
                point(out, p);
            }
        }
        GeomType::LineString | GeomType::Polygon => {
            let polygon = feature.geom_type == GeomType::Polygon;
            let min = if polygon { 3 } else { 2 };
            for part in feature.parts.iter().filter(|p| p.len() >= min) {
                out.push(command(CMD_MOVE_TO, 1));
                point(out, part[0]);
                out.push(command(CMD_LINE_TO, part.len() - 1));
                for &p in &part[1..] {
                    point(out, p);
                }
                if polygon {
                    out.push(command(CMD_CLOSE_PATH, 1));
                }
            }
        }
    }
}

/// Encode layers into an MVT tile. Empty layers are omitted; returns an
/// empty vector if nothing remains.
pub fn encode_tile(layers: &[TileLayer]) -> Vec<u8> {
    let mut tile = Vec::new();
    let mut geometry = Vec::new();
    for layer in layers.iter().filter(|l| !l.features.is_empty()) {
        put_message(&mut tile, 3, |buf| {
            put_varint_field(buf, 15, 2); // version
            put_bytes_field(buf, 1, layer.name.as_bytes());
            let mut keys: Vec<&str> = Vec::new();
            let mut values: Vec<&str> = Vec::new();
            let mut key_index: HashMap<&str, u32> = HashMap::new();
            let mut value_index: HashMap<&str, u32> = HashMap::new();
            for f in &layer.features {
                geometry.clear();
                encode_geometry(f, &mut geometry);
                if geometry.is_empty() {
                    continue;
                }
                let mut tags = Vec::with_capacity(f.attributes.len() * 2);
                for (k, v) in &f.attributes {
                    let ki = *key_index.entry(k).or_insert_with(|| {
                        keys.push(k);
                        keys.len() as u32 - 1
                    });
                    let vi = *value_index.entry(v).or_insert_with(|| {
                        values.push(v);
                        values.len() as u32 - 1
                    });
                    tags.push(u64::from(ki));
                    tags.push(u64::from(vi));
                }
                put_message(buf, 2, |fb| {
                    if let Some(id) = f.id {
                        put_varint_field(fb, 1, id);
                    }
                    put_packed_varints(fb, 2, tags);
                    put_varint_field(fb, 3, f.geom_type as u64);
                    put_packed_varints(fb, 4, geometry.iter().copied());
                });
            }
            for k in keys {
                put_bytes_field(buf, 3, k.as_bytes());
            }
            for v in values {
                put_message(buf, 4, |vb| put_bytes_field(vb, 1, v.as_bytes()));
            }
            put_varint_field(buf, 5, u64::from(layer.extent));
        });
    }
    tile
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(parts: Vec<Vec<[i32; 2]>>) -> TileFeature {
        TileFeature {
            id: None,
            geom_type: GeomType::Point,
            parts,
            attributes: vec![],
        }
    }

    #[test]
    fn known_geometry_encodings_from_the_spec() {
        // MVT 2.1 spec §4.3.5 examples.
        let mut g = Vec::new();
        encode_geometry(&point(vec![vec![[25, 17]]]), &mut g);
        assert_eq!(g, [9, 50, 34]);

        g.clear();
        encode_geometry(&point(vec![vec![[5, 7], [3, 2]]]), &mut g);
        assert_eq!(g, [17, 10, 14, 3, 9]);

        let line = TileFeature {
            geom_type: GeomType::LineString,
            ..point(vec![vec![[2, 2], [2, 10], [10, 10]]])
        };
        g.clear();
        encode_geometry(&line, &mut g);
        assert_eq!(g, [9, 4, 4, 18, 0, 16, 16, 0]);

        let polygon = TileFeature {
            geom_type: GeomType::Polygon,
            ..point(vec![vec![[3, 6], [8, 12], [20, 34]]])
        };
        g.clear();
        encode_geometry(&polygon, &mut g);
        assert_eq!(g, [9, 6, 12, 18, 10, 12, 24, 44, 15]);
    }

    #[test]
    fn cursor_carries_across_parts() {
        // Spec §4.3.5.4 multilinestring example.
        let f = TileFeature {
            geom_type: GeomType::LineString,
            ..point(vec![vec![[2, 2], [2, 10], [10, 10]], vec![[1, 1], [3, 5]]])
        };
        let mut g = Vec::new();
        encode_geometry(&f, &mut g);
        assert_eq!(g, [9, 4, 4, 18, 0, 16, 16, 0, 9, 17, 17, 10, 4, 8]);
    }

    #[test]
    fn keys_and_values_are_deduplicated() {
        let f = |name: &str| TileFeature {
            id: Some(1),
            attributes: vec![
                ("class".into(), "cafe".into()),
                ("name".into(), name.into()),
            ],
            ..point(vec![vec![[1, 1]]])
        };
        let tile = encode_tile(&[TileLayer {
            name: "amenity".into(),
            extent: 4096,
            features: vec![f("A"), f("B"), f("A")],
        }]);
        let layers = crate::mvt_decode::decode_layers(&tile).expect("valid");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].features.len(), 3);
        let tables = crate::mvt_decode::layer_tables(&tile).expect("valid");
        assert_eq!(tables[0].keys, ["class", "name"]);
        assert_eq!(tables[0].values, ["cafe", "A", "B"]);
    }

    #[test]
    fn empty_layers_are_omitted() {
        assert!(
            encode_tile(&[TileLayer {
                name: "x".into(),
                extent: 4096,
                features: vec![]
            }])
            .is_empty()
        );
    }
}
