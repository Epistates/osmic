//! In-memory model of a vector tile in tile-local integer coordinates.
//!
//! This is the common currency of the tile pipeline: the renderer produces
//! [`TileFeature`]s, the generator's external sort stores them, and the
//! MVT/MLT encoders and the MVT decoder consume or produce them.

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

#[cfg(test)]
mod tests {
    use super::*;

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
        use crate::proto::{
            put_bytes_field, put_message, put_varint, put_varint_field, zigzag_encode32,
        };
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
