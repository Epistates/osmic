//! Mapbox Vector Tile decoder, safe on untrusted input.
//!
//! Every read is bounds-checked, coordinate arithmetic is overflow-checked,
//! and malformed data yields a [`DecodeError`] — never a panic or a hang.
//! Multi-geometries are preserved, and polygon rings are grouped into
//! polygons by winding order as MVT 2.1 requires (positive area = new
//! exterior, negative = hole of the preceding exterior).

use std::fmt;

use geo_types::{Coord, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};

use osmic_core::mercator::{unit_x_to_lon, unit_y_to_lat};
use osmic_core::{Geometry, TileCoord};

use crate::model::{GeomType, TileFeature, TileLayer, ring_area2};
pub use crate::proto::DecodeError;
use crate::proto::{Field, Reader, packed_varints, zigzag_decode32};

fn invalid(what: &'static str, detail: impl Into<String>) -> DecodeError {
    DecodeError::Invalid {
        what,
        detail: detail.into(),
    }
}

/// A feature attribute value with the type it has in the tile.
///
/// Keeping the type lets style filters compare numbers with numbers and
/// booleans with booleans, as MapLibre does. [`fmt::Display`] gives the
/// value's text (`1.5`, `-3`, `true`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AttrValue {
    /// `string_value`.
    String(String),
    /// `float_value`.
    Float(f32),
    /// `double_value`.
    Double(f64),
    /// `int_value` or `sint_value`.
    Int(i64),
    /// `uint_value`.
    UInt(u64),
    /// `bool_value`.
    Bool(bool),
}

impl AttrValue {
    /// The string, if this is a string value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value as a double, if it is numeric (64-bit integers beyond
    /// 2^53 lose precision, as in JavaScript).
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Float(v) => Some(f64::from(v)),
            Self::Double(v) => Some(v),
            Self::Int(v) => Some(v as f64),
            Self::UInt(v) => Some(v as f64),
            Self::String(_) | Self::Bool(_) => None,
        }
    }

    /// The boolean, if this is a boolean value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

impl fmt::Display for AttrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => f.write_str(s),
            Self::Float(v) => write!(f, "{v}"),
            Self::Double(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::UInt(v) => write!(f, "{v}"),
            Self::Bool(v) => write!(f, "{v}"),
        }
    }
}

impl From<String> for AttrValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for AttrValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        Self::Double(v)
    }
}

impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// A decoded feature in geographic coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFeature {
    /// Name of the tile layer the feature belongs to.
    pub layer: String,
    /// The feature id, if the tile has one.
    pub id: Option<u64>,
    /// The `class` attribute, if present and a string.
    pub class: Option<String>,
    /// The `name` attribute, if present and a string.
    pub name: Option<String>,
    /// All other attributes, typed. (A non-string `class` or `name` is
    /// kept here too.)
    pub tags: Vec<(String, AttrValue)>,
    /// WGS84 geometry (polygon exteriors counter-clockwise).
    pub geometry: Geometry,
}

impl DecodedFeature {
    /// The attribute `key` (including `class` and `name`), if present.
    pub fn attribute(&self, key: &str) -> Option<AttrRef<'_>> {
        let special = match key {
            "class" => self.class.as_deref(),
            "name" => self.name.as_deref(),
            _ => None,
        };
        special.map(AttrRef::String).or_else(|| {
            self.tags
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| AttrRef::from(v))
        })
    }
}

/// A borrowed attribute value, from [`DecodedFeature::attribute`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum AttrRef<'a> {
    /// A string.
    String(&'a str),
    /// Any numeric value, as a double.
    Number(f64),
    /// A boolean.
    Bool(bool),
}

impl<'a> From<&'a AttrValue> for AttrRef<'a> {
    fn from(v: &'a AttrValue) -> Self {
        match v {
            AttrValue::String(s) => Self::String(s),
            AttrValue::Bool(b) => Self::Bool(*b),
            other => Self::Number(other.as_f64().unwrap_or(f64::NAN)),
        }
    }
}

struct RawLayer<'a> {
    name: String,
    extent: u32,
    keys: Vec<String>,
    values: Vec<AttrValue>,
    features: Vec<&'a [u8]>,
}

fn utf8(bytes: &[u8], what: &'static str) -> Result<String, DecodeError> {
    String::from_utf8(bytes.to_vec()).map_err(|e| invalid(what, e.to_string()))
}

fn decode_value(data: &[u8]) -> Result<AttrValue, DecodeError> {
    let mut r = Reader::new(data);
    let mut out = None;
    while let Some((field, value)) = r.next_field()? {
        out = Some(match (field, value) {
            (1, Field::Bytes(b)) => AttrValue::String(utf8(b, "string value")?),
            (2, Field::Fixed32(bits)) => AttrValue::Float(f32::from_bits(bits)),
            (3, Field::Fixed64(bits)) => AttrValue::Double(f64::from_bits(bits)),
            (4, Field::Varint(v)) => AttrValue::Int(v as i64),
            (5, Field::Varint(v)) => AttrValue::UInt(v),
            (6, Field::Varint(v)) => AttrValue::Int((v >> 1) as i64 ^ -((v & 1) as i64)),
            (7, Field::Varint(v)) => AttrValue::Bool(v != 0),
            _ => continue,
        });
    }
    out.ok_or_else(|| invalid("value", "no supported value field"))
}

fn raw_layers(data: &[u8]) -> Result<Vec<RawLayer<'_>>, DecodeError> {
    let mut layers = Vec::new();
    let mut r = Reader::new(data);
    while let Some((field, value)) = r.next_field()? {
        let (3, Field::Bytes(layer)) = (field, value) else {
            continue;
        };
        let mut raw = RawLayer {
            name: String::new(),
            extent: 4096,
            keys: Vec::new(),
            values: Vec::new(),
            features: Vec::new(),
        };
        let mut lr = Reader::new(layer);
        while let Some((field, value)) = lr.next_field()? {
            match (field, value) {
                (1, Field::Bytes(b)) => raw.name = utf8(b, "layer name")?,
                (2, Field::Bytes(b)) => raw.features.push(b),
                (3, Field::Bytes(b)) => raw.keys.push(utf8(b, "key")?),
                (4, Field::Bytes(b)) => raw.values.push(decode_value(b)?),
                (5, Field::Varint(v)) => {
                    raw.extent = u32::try_from(v)
                        .ok()
                        .filter(|&e| e > 0)
                        .ok_or_else(|| invalid("extent", v.to_string()))?;
                }
                _ => {}
            }
        }
        layers.push(raw);
    }
    Ok(layers)
}

/// Decode a geometry command stream into parts.
fn decode_commands(geom_type: GeomType, data: &[u8]) -> Result<Vec<Vec<[i32; 2]>>, DecodeError> {
    let mut ints = packed_varints(data);
    let mut next_u32 = || -> Result<Option<u32>, DecodeError> {
        ints.next()
            .transpose()?
            .map(|v| u32::try_from(v).map_err(|_| invalid("geometry", "value exceeds u32")))
            .transpose()
    };
    let (mut cx, mut cy) = (0i64, 0i64);
    let mut parts: Vec<Vec<[i32; 2]>> = Vec::new();
    let mut current: Vec<[i32; 2]> = Vec::new();
    while let Some(cmd) = next_u32()? {
        let (id, count) = (cmd & 7, (cmd >> 3) as usize);
        match id {
            1 | 2 => {
                if id == 1 && geom_type != GeomType::Point && !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                for _ in 0..count {
                    let (Some(dx), Some(dy)) = (next_u32()?, next_u32()?) else {
                        return Err(invalid("geometry", "command count exceeds parameters"));
                    };
                    cx += i64::from(zigzag_decode32(dx));
                    cy += i64::from(zigzag_decode32(dy));
                    let x =
                        i32::try_from(cx).map_err(|_| invalid("geometry", "x overflows i32"))?;
                    let y =
                        i32::try_from(cy).map_err(|_| invalid("geometry", "y overflows i32"))?;
                    current.push([x, y]);
                }
            }
            7 => {
                if geom_type != GeomType::Polygon {
                    return Err(invalid("geometry", "ClosePath outside a polygon"));
                }
                parts.push(std::mem::take(&mut current));
            }
            other => return Err(invalid("geometry", format!("unknown command {other}"))),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

/// A feature's geometry (`attributes` left empty) and typed attributes.
type RawFeature = (TileFeature, Vec<(String, AttrValue)>);

fn decode_feature(raw: &RawLayer<'_>, data: &[u8]) -> Result<Option<RawFeature>, DecodeError> {
    let mut r = Reader::new(data);
    let (mut id, mut geom_type, mut geometry, mut tags) = (None, None, &[][..], &[][..]);
    while let Some((field, value)) = r.next_field()? {
        match (field, value) {
            (1, Field::Varint(v)) => id = Some(v),
            (2, Field::Bytes(b)) => tags = b,
            (3, Field::Varint(v)) => geom_type = u8::try_from(v).ok().and_then(GeomType::from_u8),
            (4, Field::Bytes(b)) => geometry = b,
            _ => {}
        }
    }
    // Unknown geometry type: the spec allows decoders to ignore the feature.
    let Some(geom_type) = geom_type else {
        return Ok(None);
    };
    let mut attributes = Vec::new();
    let mut it = packed_varints(tags);
    while let Some(k) = it.next() {
        let v = it
            .next()
            .ok_or_else(|| invalid("tags", "odd number of tag indices"))??;
        let k = k?;
        let key = usize::try_from(k).ok().and_then(|k| raw.keys.get(k));
        let value = usize::try_from(v).ok().and_then(|v| raw.values.get(v));
        match (key, value) {
            (Some(key), Some(value)) => attributes.push((key.clone(), value.clone())),
            _ => return Err(invalid("tags", format!("index {k}/{v} out of range"))),
        }
    }
    let parts = decode_commands(geom_type, geometry)?;
    let feature = TileFeature {
        id,
        geom_type,
        parts,
        attributes: Vec::new(),
    };
    Ok(Some((feature, attributes)))
}

/// Decode an MVT tile into tile-local layers. Attribute values become
/// their text (see [`AttrValue`]'s `Display`); [`decode_tile`] keeps them
/// typed.
pub fn decode_layers(data: &[u8]) -> Result<Vec<TileLayer>, DecodeError> {
    raw_layers(data)?
        .into_iter()
        .map(|raw| {
            let features = raw
                .features
                .iter()
                .filter_map(|f| decode_feature(&raw, f).transpose())
                .map(|f| {
                    f.map(|(mut feature, attributes)| {
                        feature.attributes = attributes
                            .into_iter()
                            .map(|(k, v)| (k, v.to_string()))
                            .collect();
                        feature
                    })
                })
                .collect::<Result<_, _>>()?;
            Ok(TileLayer {
                name: raw.name,
                extent: raw.extent,
                features,
            })
        })
        .collect()
}

/// A layer's attribute dictionaries, in encoded order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerTables {
    /// The layer name.
    pub name: String,
    /// The key table.
    pub keys: Vec<String>,
    /// The value table, as text.
    pub values: Vec<String>,
}

/// Each layer's key and value tables (for inspection and testing).
pub fn layer_tables(data: &[u8]) -> Result<Vec<LayerTables>, DecodeError> {
    Ok(raw_layers(data)?
        .into_iter()
        .map(|l| LayerTables {
            name: l.name,
            keys: l.keys,
            values: l.values.iter().map(ToString::to_string).collect(),
        })
        .collect())
}

/// Convert a tile-local feature to WGS84 geometry for `tile`.
pub fn to_geographic(feature: &TileFeature, extent: u32, tile: TileCoord) -> Option<Geometry> {
    let n = f64::from(osmic_core::mercator::tiles_per_axis(tile.z.0));
    let e = f64::from(extent);
    let to = |[x, y]: [i32; 2]| Coord {
        x: unit_x_to_lon((f64::from(tile.x) + f64::from(x) / e) / n),
        y: unit_y_to_lat((f64::from(tile.y) + f64::from(y) / e) / n),
    };
    match feature.geom_type {
        GeomType::Point => {
            let pts: Vec<Point<f64>> = feature
                .parts
                .iter()
                .flatten()
                .map(|&p| Point(to(p)))
                .collect();
            match pts.len() {
                0 => None,
                1 => Some(Geometry::Point(pts[0])),
                _ => Some(Geometry::MultiPoint(MultiPoint(pts))),
            }
        }
        GeomType::LineString => {
            let lines: Vec<LineString<f64>> = feature
                .parts
                .iter()
                .filter(|p| p.len() >= 2)
                .map(|p| LineString(p.iter().map(|&c| to(c)).collect()))
                .collect();
            match lines.len() {
                0 => None,
                1 => lines.into_iter().next().map(Geometry::Line),
                _ => Some(Geometry::MultiLine(MultiLineString(lines))),
            }
        }
        GeomType::Polygon => {
            let mut polys: Vec<(LineString<f64>, Vec<LineString<f64>>)> = Vec::new();
            for ring in feature.parts.iter().filter(|r| r.len() >= 3) {
                let area = ring_area2(ring);
                if area == 0 {
                    continue;
                }
                let mut ls: Vec<Coord<f64>> = ring.iter().map(|&c| to(c)).collect();
                ls.push(ls[0]);
                // An MVT exterior (positive area, y down) is clockwise as
                // drawn — on screen and on a north-up map alike. OGC wants
                // exteriors counter-clockwise and holes clockwise, so both
                // are reversed.
                match polys.last_mut() {
                    Some((_, holes)) if area < 0 => {
                        ls.reverse();
                        holes.push(LineString(ls));
                    }
                    // A hole with no exterior before it: tolerate it as an
                    // exterior; it is already counter-clockwise.
                    _ if area < 0 => polys.push((LineString(ls), Vec::new())),
                    _ => {
                        ls.reverse();
                        polys.push((LineString(ls), Vec::new()));
                    }
                }
            }
            let mut polys: Vec<Polygon<f64>> = polys
                .into_iter()
                .map(|(ext, holes)| Polygon::new(ext, holes))
                .collect();
            match polys.len() {
                0 => None,
                1 => Some(Geometry::Polygon(polys.remove(0))),
                _ => Some(Geometry::MultiPolygon(MultiPolygon(polys))),
            }
        }
    }
}

/// Decode an MVT tile into WGS84 features.
pub fn decode_tile(data: &[u8], tile: TileCoord) -> Result<Vec<DecodedFeature>, DecodeError> {
    let mut out = Vec::new();
    for raw in raw_layers(data)? {
        for data in &raw.features {
            let Some((f, attributes)) = decode_feature(&raw, data)? else {
                continue;
            };
            let Some(geometry) = to_geographic(&f, raw.extent, tile) else {
                continue;
            };
            let mut class = None;
            let mut name = None;
            let mut tags = Vec::with_capacity(attributes.len());
            for (k, v) in attributes {
                let v = match (k.as_str(), v) {
                    ("class", AttrValue::String(s)) => {
                        class = Some(s);
                        continue;
                    }
                    ("name", AttrValue::String(s)) => {
                        name = Some(s);
                        continue;
                    }
                    (_, v) => v,
                };
                tags.push((k, v));
            }
            out.push(DecodedFeature {
                layer: raw.name.clone(),
                id: f.id,
                class,
                name,
                tags,
                geometry,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::encode_tile;
    use geo::Winding;
    use osmic_core::Zoom;

    fn layer(features: Vec<TileFeature>) -> Vec<u8> {
        encode_tile(&[TileLayer {
            name: "test".into(),
            extent: 4096,
            features,
        }])
    }

    #[test]
    fn round_trip_every_geometry_type() {
        let features = vec![
            TileFeature {
                id: Some(12),
                geom_type: GeomType::Point,
                parts: vec![vec![[1, 2], [300, 400]]],
                attributes: vec![("class".into(), "x".into())],
            },
            TileFeature {
                id: None,
                geom_type: GeomType::LineString,
                parts: vec![vec![[0, 0], [10, 10]], vec![[-64, 5], [4160, 5]]],
                attributes: vec![],
            },
            TileFeature {
                id: Some(3),
                geom_type: GeomType::Polygon,
                parts: vec![
                    vec![[0, 0], [100, 0], [100, 100], [0, 100]],
                    vec![[10, 10], [10, 20], [20, 20], [20, 10]],
                    vec![[200, 200], [300, 200], [300, 300], [200, 300]],
                ],
                attributes: vec![("name".into(), "n".into())],
            },
        ];
        let tile = layer(features.clone());
        let decoded = decode_layers(&tile).expect("valid");
        assert_eq!(decoded[0].features, features);
    }

    #[test]
    fn geographic_decoding_keeps_multi_geometries_and_holes() {
        let features = vec![
            TileFeature {
                id: None,
                geom_type: GeomType::LineString,
                parts: vec![vec![[0, 0], [10, 10]], vec![[20, 20], [30, 30]]],
                attributes: vec![],
            },
            TileFeature {
                id: None,
                geom_type: GeomType::Polygon,
                parts: vec![
                    vec![[0, 0], [100, 0], [100, 100], [0, 100]],
                    vec![[10, 10], [10, 20], [20, 20], [20, 10]],
                    vec![[200, 200], [300, 200], [300, 300], [200, 300]],
                ],
                attributes: vec![],
            },
        ];
        let tile = layer(features);
        let decoded = decode_tile(&tile, TileCoord::new(1, 1, Zoom(2))).expect("valid");
        assert!(matches!(&decoded[0].geometry, Geometry::MultiLine(m) if m.0.len() == 2));
        let Geometry::MultiPolygon(mp) = &decoded[1].geometry else {
            panic!("expected multipolygon, got {:?}", decoded[1].geometry);
        };
        assert_eq!(
            mp.0.len(),
            2,
            "two exteriors are two polygons, not one with a hole"
        );
        assert_eq!(mp.0[0].interiors().len(), 1);
        assert!(mp.0[0].exterior().is_ccw());
        assert!(mp.0[0].interiors()[0].is_cw());
    }

    #[test]
    fn sint_values_are_zigzag_decoded() {
        // Value message with sint_value (field 6) = -3 → zigzag 5.
        assert_eq!(decode_value(&[0x30, 5]), Ok(AttrValue::Int(-3)));
        assert_eq!(decode_value(&[0x38, 1]), Ok(AttrValue::Bool(true)));
    }

    /// One point feature whose attributes use every MVT value type.
    fn typed_tile() -> Vec<u8> {
        use crate::proto::{put_bytes_field, put_message, put_packed_varints, put_varint_field};
        let values: Vec<Vec<u8>> = vec![
            {
                let mut v = Vec::new();
                put_bytes_field(&mut v, 1, b"primary");
                v
            },
            {
                let mut v = Vec::new();
                v.push((2 << 3) | 5); // float_value, fixed32
                v.extend_from_slice(&1.5f32.to_bits().to_le_bytes());
                v
            },
            {
                let mut v = Vec::new();
                crate::proto::put_fixed64_field(&mut v, 3, 2.25f64.to_bits());
                v
            },
            {
                let mut v = Vec::new();
                put_varint_field(&mut v, 4, 2);
                v
            },
            {
                let mut v = Vec::new();
                put_varint_field(&mut v, 5, 7);
                v
            },
            {
                let mut v = Vec::new();
                put_varint_field(&mut v, 6, 5); // sint -3
                v
            },
            {
                let mut v = Vec::new();
                put_varint_field(&mut v, 7, 1);
                v
            },
            {
                let mut v = Vec::new();
                put_varint_field(&mut v, 4, 9); // a numeric `name`
                v
            },
        ];
        let keys = [
            "class",
            "width",
            "area",
            "admin_level",
            "rank",
            "offset",
            "oneway",
            "name",
        ];
        let mut tile = Vec::new();
        put_message(&mut tile, 3, |layer| {
            put_bytes_field(layer, 1, b"test");
            put_message(layer, 2, |f| {
                put_packed_varints(f, 2, (0..keys.len() as u64).flat_map(|i| [i, i]));
                put_varint_field(f, 3, 1); // point
                put_packed_varints(f, 4, [1 | (1 << 3), 2, 2]);
            });
            for k in keys {
                put_bytes_field(layer, 3, k.as_bytes());
            }
            for v in &values {
                put_bytes_field(layer, 4, v);
            }
            put_varint_field(layer, 5, 4096);
        });
        tile
    }

    #[test]
    fn attribute_types_are_preserved() {
        let decoded = decode_tile(&typed_tile(), TileCoord::new(0, 0, Zoom(0))).unwrap();
        let f = &decoded[0];
        assert_eq!(f.class.as_deref(), Some("primary"));
        assert_eq!(f.name, None, "a numeric name stays a typed attribute");
        let attr = |k: &str| f.attribute(k);
        assert_eq!(attr("class"), Some(AttrRef::String("primary")));
        assert_eq!(attr("width"), Some(AttrRef::Number(1.5)));
        assert_eq!(attr("area"), Some(AttrRef::Number(2.25)));
        assert_eq!(attr("admin_level"), Some(AttrRef::Number(2.0)));
        assert_eq!(attr("rank"), Some(AttrRef::Number(7.0)));
        assert_eq!(attr("offset"), Some(AttrRef::Number(-3.0)));
        assert_eq!(attr("oneway"), Some(AttrRef::Bool(true)));
        assert_eq!(attr("name"), Some(AttrRef::Number(9.0)));
        assert_eq!(attr("missing"), None);
        // The tile-local decoder and the tables still give text.
        let layers = decode_layers(&typed_tile()).unwrap();
        let text: Vec<&str> = layers[0].features[0]
            .attributes
            .iter()
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            text,
            ["primary", "1.5", "2.25", "2", "7", "-3", "true", "9"]
        );
        assert_eq!(layer_tables(&typed_tile()).unwrap()[0].values[6], "true");
    }

    #[test]
    fn hostile_inputs_error_without_panicking_or_hanging() {
        let cases: &[&[u8]] = &[
            // Layer whose feature declares a tag list longer than the data
            // (the original infinite loop).
            &[0x1a, 0x06, 0x12, 0x04, 0x12, 0x7f, 0x01, 0x02],
            // Layer length beyond the buffer.
            &[0x1a, 0xff, 0x01, 0x00],
            // Varint that never terminates.
            &[
                0x1a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ],
            // Extent 0.
            &[0x1a, 0x02, 0x28, 0x00],
        ];
        for data in cases {
            assert!(decode_layers(data).is_err(), "{data:?}");
        }
        // Coordinate overflow: MoveTo(2) with deltas of +i32::MAX each.
        let mut geom = Vec::new();
        crate::proto::put_varint(&mut geom, 1 | (2 << 3));
        for _ in 0..4 {
            crate::proto::put_varint(&mut geom, u64::from(u32::MAX - 1));
        }
        assert!(matches!(
            decode_commands(GeomType::Point, &geom),
            Err(DecodeError::Invalid { .. })
        ));
    }

    #[test]
    fn random_bytes_never_panic() {
        // Deterministic xorshift fuzz; real fuzzing lives in the fuzz targets.
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        for len in 0..2_000usize {
            let data: Vec<u8> = (0..len % 97)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    s as u8
                })
                .collect();
            let _ = decode_layers(&data);
            let _ = decode_tile(&data, TileCoord::new(0, 0, Zoom(0)));
        }
    }
}
