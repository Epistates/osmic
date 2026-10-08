//! In-memory model of a vector tile in tile-local integer coordinates.
//!
//! This is the common currency of the tile pipeline: the renderer produces
//! [`TileFeature`]s, the external sort stores them, and the MVT/MLT encoders
//! and the MVT decoder consume or produce them.

use crate::proto::{DecodeError, Reader, put_varint, zigzag_decode32, zigzag_encode32};

/// The three vector-tile geometry types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum GeomType {
    Point = 1,
    LineString = 2,
    Polygon = 3,
}

impl GeomType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Point),
            2 => Some(Self::LineString),
            3 => Some(Self::Polygon),
            _ => None,
        }
    }
}

/// A feature in tile-local integer coordinates (origin at the tile's
/// top-left corner, y growing downward, `extent` units per tile side;
/// coordinates may lie in the buffer outside `0..extent`).
///
/// `parts` layout by geometry type:
/// - **Point**: one part containing every point.
/// - **LineString**: one part per line (each ≥ 2 points).
/// - **Polygon**: rings without the repeated closing vertex. A ring with
///   positive surveyor-formula area (clockwise on screen) starts a new
///   polygon; negative rings are holes of the preceding exterior — the MVT
///   2.1 convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileFeature {
    pub id: Option<u64>,
    pub geom_type: GeomType,
    pub parts: Vec<Vec<[i32; 2]>>,
    pub attributes: Vec<(String, String)>,
}

/// One layer of a tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileLayer {
    pub name: String,
    pub extent: u32,
    pub features: Vec<TileFeature>,
}

/// Twice the signed area of an open ring in tile coordinates (surveyor's
/// formula). Positive = exterior in MVT terms.
///
/// Exact for any `i32` coordinates: a single cross product of two `i32`
/// points can reach 2⁶³, so the sum is kept in `i128` (a ring would need
/// more than 2⁶³ vertices to overflow it).
pub fn ring_area2(ring: &[[i32; 2]]) -> i128 {
    let n = ring.len();
    let mut sum = 0i128;
    for i in 0..n {
        let [x0, y0] = ring[i];
        let [x1, y1] = ring[(i + 1) % n];
        sum += i128::from(x0) * i128::from(y1) - i128::from(x1) * i128::from(y0);
    }
    sum
}

/// The leading part of a [`TileFeature`] sort record: id, geometry type and
/// geometry. The record continues with [`encode_record_attributes`], which
/// is identical for every piece of a feature and so can be encoded once and
/// appended to each.
pub(crate) fn encode_record_geometry(
    id: Option<u64>,
    geom_type: GeomType,
    parts: &[Vec<[i32; 2]>],
    out: &mut Vec<u8>,
) {
    match id {
        Some(id) => {
            out.push(1);
            put_varint(out, id);
        }
        None => out.push(0),
    }
    out.push(geom_type as u8);
    put_varint(out, parts.len() as u64);
    for part in parts {
        put_varint(out, part.len() as u64);
        let (mut px, mut py) = (0i32, 0i32);
        for &[x, y] in part {
            put_varint(out, u64::from(zigzag_encode32(x.wrapping_sub(px))));
            put_varint(out, u64::from(zigzag_encode32(y.wrapping_sub(py))));
            (px, py) = (x, y);
        }
    }
}

/// The trailing part of a sort record; see [`encode_record_geometry`].
pub(crate) fn encode_record_attributes(attributes: &[(String, String)], out: &mut Vec<u8>) {
    put_varint(out, attributes.len() as u64);
    for (k, v) in attributes {
        put_varint(out, k.len() as u64);
        out.extend_from_slice(k.as_bytes());
        put_varint(out, v.len() as u64);
        out.extend_from_slice(v.as_bytes());
    }
}

impl TileFeature {
    /// Serialise for the external sort (compact varint encoding).
    pub fn encode(&self, out: &mut Vec<u8>) {
        encode_record_geometry(self.id, self.geom_type, &self.parts, out);
        encode_record_attributes(&self.attributes, out);
    }

    /// Inverse of [`TileFeature::encode`].
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let invalid = |detail: &str| DecodeError::Invalid {
            what: "tile feature record",
            detail: detail.to_string(),
        };
        let mut r = Reader::new(data);
        let byte = |r: &mut Reader<'_>| -> Result<u8, DecodeError> { Ok(r.bytes(1)?[0]) };
        let id = match byte(&mut r)? {
            0 => None,
            _ => Some(r.varint()?),
        };
        let geom_type = GeomType::from_u8(byte(&mut r)?).ok_or_else(|| invalid("geometry type"))?;
        let count = |r: &mut Reader<'_>| -> Result<usize, DecodeError> {
            let n = r.varint()?;
            // Every element takes at least one byte, so a count larger than
            // the record is corrupt (and must not drive an allocation).
            usize::try_from(n)
                .ok()
                .filter(|&n| n <= data.len())
                .ok_or_else(|| invalid("count exceeds record length"))
        };
        let n_parts = count(&mut r)?;
        let mut parts = Vec::with_capacity(n_parts);
        for _ in 0..n_parts {
            let n = count(&mut r)?;
            let mut part = Vec::with_capacity(n);
            let (mut x, mut y) = (0i32, 0i32);
            for _ in 0..n {
                let dx = u32::try_from(r.varint()?).map_err(|_| invalid("delta"))?;
                let dy = u32::try_from(r.varint()?).map_err(|_| invalid("delta"))?;
                x = x.wrapping_add(zigzag_decode32(dx));
                y = y.wrapping_add(zigzag_decode32(dy));
                part.push([x, y]);
            }
            parts.push(part);
        }
        let n_attrs = count(&mut r)?;
        let mut attributes = Vec::with_capacity(n_attrs);
        let string = |r: &mut Reader<'_>| -> Result<String, DecodeError> {
            let n = count(r)?;
            let bytes = r.bytes(n)?;
            String::from_utf8(bytes.to_vec()).map_err(|_| invalid("utf-8"))
        };
        for _ in 0..n_attrs {
            let k = string(&mut r)?;
            let v = string(&mut r)?;
            attributes.push((k, v));
        }
        Ok(Self {
            id,
            geom_type,
            parts,
            attributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TileFeature {
        TileFeature {
            id: Some(422),
            geom_type: GeomType::Polygon,
            parts: vec![
                vec![[0, 0], [100, 0], [100, 100], [0, 100]],
                vec![[10, 10], [10, 20], [20, 20], [20, 10]],
                vec![[-64, 4100], [4160, -64], [i32::MAX, i32::MIN]],
            ],
            attributes: vec![
                ("class".into(), "grass".into()),
                ("name".into(), "Ünïcode ✓".into()),
            ],
        }
    }

    #[test]
    fn record_round_trip() {
        let f = sample();
        let mut buf = Vec::new();
        f.encode(&mut buf);
        assert_eq!(TileFeature::decode(&buf), Ok(f));
        let point = TileFeature {
            id: None,
            geom_type: GeomType::Point,
            parts: vec![vec![[1, 2]]],
            attributes: vec![],
        };
        let mut buf = Vec::new();
        point.encode(&mut buf);
        assert_eq!(TileFeature::decode(&buf), Ok(point));
    }

    #[test]
    fn truncated_records_error_instead_of_panicking() {
        let mut buf = Vec::new();
        sample().encode(&mut buf);
        for len in 0..buf.len() {
            assert!(TileFeature::decode(&buf[..len]).is_err(), "prefix {len}");
        }
    }

    /// A ring around the whole `i32` plane, reached in steps a decoder
    /// accepts (each delta fits in `i32`).
    fn huge_ring() -> Vec<[i32; 2]> {
        let m = i32::MAX;
        vec![
            [m, -m],
            [0, -m],
            [-m, -m],
            [-m, 0],
            [-m, m],
            [0, m],
            [m, m],
            [m, 0],
        ]
    }

    #[test]
    fn ring_area_is_exact_at_the_i32_limits() {
        let m = i128::from(i32::MAX);
        // Side 2m, counter-clockwise on screen: negative.
        assert_eq!(ring_area2(&huge_ring()), -2 * (2 * m) * (2 * m));
        let mut reversed = huge_ring();
        reversed.reverse();
        assert_eq!(ring_area2(&reversed), 2 * (2 * m) * (2 * m));
        let corners = [
            [i32::MIN, i32::MIN],
            [i32::MAX, i32::MIN],
            [i32::MAX, i32::MAX],
        ];
        assert!(ring_area2(&corners) > 0);
    }

    #[test]
    fn decoding_a_polygon_at_the_i32_limits_does_not_overflow() {
        use crate::proto::{put_bytes_field, put_message, put_varint, put_varint_field};
        let ring = huge_ring();
        let mut geometry = Vec::new();
        let (mut cx, mut cy) = (0i32, 0i32);
        let mut delta = |out: &mut Vec<u8>, [x, y]: [i32; 2]| {
            put_varint(out, u64::from(zigzag_encode32(x - cx)));
            put_varint(out, u64::from(zigzag_encode32(y - cy)));
            (cx, cy) = (x, y);
        };
        put_varint(&mut geometry, 1 | (1 << 3)); // MoveTo(1)
        delta(&mut geometry, ring[0]);
        put_varint(&mut geometry, 2 | ((ring.len() as u64 - 1) << 3)); // LineTo
        for &p in &ring[1..] {
            delta(&mut geometry, p);
        }
        put_varint(&mut geometry, 7 | (1 << 3)); // ClosePath
        let mut tile = Vec::new();
        put_message(&mut tile, 3, |layer| {
            put_varint_field(layer, 15, 2);
            put_bytes_field(layer, 1, b"huge");
            put_message(layer, 2, |f| {
                put_varint_field(f, 3, GeomType::Polygon as u64);
                put_bytes_field(f, 4, &geometry);
            });
            put_varint_field(layer, 5, 4096);
        });
        let layers = crate::mvt_decode::decode_layers(&tile).expect("valid tile");
        assert_eq!(layers[0].features[0].parts, [ring]);
        let features = crate::mvt_decode::decode_tile(
            &tile,
            osmic_core::TileCoord::new(0, 0, osmic_core::Zoom::MIN),
        )
        .expect("valid tile");
        assert_eq!(features.len(), 1);
    }

    #[test]
    fn ring_area_sign_follows_mvt_convention() {
        // Clockwise on screen (y down) → positive.
        let exterior = [[0, 0], [10, 0], [10, 10], [0, 10]];
        assert!(ring_area2(&exterior) > 0);
        let mut hole = exterior;
        hole.reverse();
        assert!(ring_area2(&hole) < 0);
    }
}
