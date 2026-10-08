//! Spherical Web Mercator (EPSG:3857) — the single projection implementation
//! used across osmic.
//!
//! Three coordinate spaces appear here:
//!
//! - **Geographic**: WGS84 longitude/latitude in degrees.
//! - **Unit**: the Mercator square normalised to `[0, 1] × [0, 1]`, with
//!   `(0, 0)` at the north-west corner (y grows southward, as in slippy-map
//!   tile coordinates). Tile `(x, y)` at zoom `z` covers
//!   `[x, x+1) / 2^z × [y, y+1) / 2^z`.
//! - **Meters**: EPSG:3857 easting/northing.

use std::f64::consts::PI;

use crate::bbox::BBox;

/// Latitude where the Web Mercator square ends: `atan(sinh(π))` in degrees.
pub const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

/// WGS84 semi-major axis, the sphere radius used by EPSG:3857.
pub const EARTH_RADIUS: f64 = 6_378_137.0;

/// Longitude (degrees) → unit x in `[0, 1]`. Not clamped.
#[inline]
pub fn lon_to_unit_x(lon: f64) -> f64 {
    (lon + 180.0) / 360.0
}

/// Latitude (degrees) → unit y in `[0, 1]`, clamped to the Mercator limit.
#[inline]
pub fn lat_to_unit_y(lat: f64) -> f64 {
    let sin = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians().sin();
    0.5 - ((1.0 + sin) / (1.0 - sin)).ln() / (4.0 * PI)
}

/// Unit x → longitude in degrees.
#[inline]
pub fn unit_x_to_lon(x: f64) -> f64 {
    x * 360.0 - 180.0
}

/// Unit y → latitude in degrees.
#[inline]
pub fn unit_y_to_lat(y: f64) -> f64 {
    (PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees()
}

/// Geographic → EPSG:3857 meters (latitude clamped to the Mercator limit).
pub fn lonlat_to_meters(lon: f64, lat: f64) -> (f64, f64) {
    let x = EARTH_RADIUS * lon.to_radians();
    let y = EARTH_RADIUS * (0.5 - lat_to_unit_y(lat)) * 2.0 * PI;
    (x, y)
}

/// EPSG:3857 meters → geographic degrees.
pub fn meters_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let lon = (x / EARTH_RADIUS).to_degrees();
    let lat = (2.0 * (y / EARTH_RADIUS).exp().atan() - PI / 2.0).to_degrees();
    (lon, lat)
}

/// Number of tiles along one axis at `zoom`.
#[inline]
pub fn tiles_per_axis(zoom: u8) -> u32 {
    1u32 << zoom.min(31)
}

/// Unit coordinate → tile index along one axis at `zoom`, clamped to the
/// valid range (`NaN` maps to 0).
#[inline]
fn unit_to_tile_index(unit: f64, zoom: u8) -> u32 {
    let n = tiles_per_axis(zoom);
    let t = (unit * f64::from(n)).floor();
    if t.is_nan() || t < 0.0 {
        0
    } else if t >= f64::from(n) {
        n - 1
    } else {
        t as u32
    }
}

/// The tile containing a geographic point at `zoom`. Points outside the
/// Mercator square are clamped to the edge tile.
pub fn lonlat_to_tile(lon: f64, lat: f64, zoom: u8) -> (u32, u32) {
    (
        unit_to_tile_index(lon_to_unit_x(lon), zoom),
        unit_to_tile_index(lat_to_unit_y(lat), zoom),
    )
}

/// Geographic bounds of tile `(x, y)` at `zoom`.
pub fn tile_bounds(x: u32, y: u32, zoom: u8) -> BBox {
    let n = f64::from(tiles_per_axis(zoom));
    BBox::new(
        unit_x_to_lon(f64::from(x) / n),
        unit_y_to_lat((f64::from(y) + 1.0) / n),
        unit_x_to_lon((f64::from(x) + 1.0) / n),
        unit_y_to_lat(f64::from(y) / n),
    )
}

/// Inclusive range of tiles at one zoom level. A range whose minimum
/// exceeds its maximum on either axis is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRange {
    /// Zoom level the range applies to.
    pub zoom: u8,
    /// Westernmost column, inclusive.
    pub min_x: u32,
    /// Northernmost row, inclusive (y grows southward).
    pub min_y: u32,
    /// Easternmost column, inclusive.
    pub max_x: u32,
    /// Southernmost row, inclusive.
    pub max_y: u32,
}

impl TileRange {
    /// Number of tiles in the range (0 if it is empty), saturating at
    /// `u64::MAX` — which only a range spanning the full `u32` grid on both
    /// axes, far beyond any real zoom level, can reach.
    pub fn len(&self) -> u64 {
        if self.is_empty() {
            return 0;
        }
        let side = |min: u32, max: u32| u64::from(max - min) + 1;
        side(self.min_x, self.max_x).saturating_mul(side(self.min_y, self.max_y))
    }

    /// Whether the range holds no tiles (its minimum exceeds its maximum on
    /// either axis).
    pub fn is_empty(&self) -> bool {
        self.min_x > self.max_x || self.min_y > self.max_y
    }

    /// Whether tile `(x, y)` lies in the range.
    pub fn contains(&self, x: u32, y: u32) -> bool {
        (self.min_x..=self.max_x).contains(&x) && (self.min_y..=self.max_y).contains(&y)
    }
}

/// Tiles at `zoom` intersecting a geographic bbox (y grows southward, so the
/// bbox's max latitude gives the minimum tile row).
pub fn bbox_to_tile_range(bbox: &BBox, zoom: u8) -> TileRange {
    let (min_x, min_y) = lonlat_to_tile(bbox.min_lon, bbox.max_lat, zoom);
    let (max_x, max_y) = lonlat_to_tile(bbox.max_lon, bbox.min_lat, zoom);
    TileRange {
        zoom,
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_projection_round_trips() {
        for &(lon, lat) in &[
            (-122.4194, 37.7749),
            (0.0, 0.0),
            (179.9, -84.0),
            (-180.0, 85.0),
        ] {
            let (x, y) = (lon_to_unit_x(lon), lat_to_unit_y(lat));
            assert!((unit_x_to_lon(x) - lon).abs() < 1e-9);
            assert!((unit_y_to_lat(y) - lat).abs() < 1e-9);
        }
    }

    #[test]
    fn mercator_limits_map_to_unit_square_edges() {
        assert!(lat_to_unit_y(MAX_LATITUDE).abs() < 1e-12);
        assert!((lat_to_unit_y(-MAX_LATITUDE) - 1.0).abs() < 1e-12);
        assert_eq!(lat_to_unit_y(90.0), lat_to_unit_y(MAX_LATITUDE));
        assert!((lat_to_unit_y(0.0) - 0.5).abs() < 1e-15);
    }

    #[test]
    fn meters_round_trip() {
        let (x, y) = lonlat_to_meters(-122.4194, 37.7749);
        let (lon, lat) = meters_to_lonlat(x, y);
        assert!((lon + 122.4194).abs() < 1e-9);
        assert!((lat - 37.7749).abs() < 1e-9);
        // Known value: lon 180° is half the equator.
        assert!((lonlat_to_meters(180.0, 0.0).0 - 20_037_508.342_789_244).abs() < 1e-6);
    }

    #[test]
    fn tile_lookup_clamps_and_handles_nan() {
        assert_eq!(lonlat_to_tile(0.0, 0.0, 0), (0, 0));
        assert_eq!(lonlat_to_tile(180.0, -90.0, 3), (7, 7));
        assert_eq!(lonlat_to_tile(-200.0, 90.0, 3), (0, 0));
        assert_eq!(lonlat_to_tile(f64::NAN, f64::NAN, 5), (0, 0));
        // San Francisco at z12.
        assert_eq!(lonlat_to_tile(-122.4194, 37.7749, 12), (655, 1583));
    }

    #[test]
    fn tile_bounds_contain_their_points() {
        let (x, y) = lonlat_to_tile(-122.4194, 37.7749, 14);
        let b = tile_bounds(x, y, 14);
        assert!(b.contains_point(-122.4194, 37.7749));
        let z0 = tile_bounds(0, 0, 0);
        assert!((z0.min_lon + 180.0).abs() < 1e-12 && (z0.max_lat - MAX_LATITUDE).abs() < 1e-9);
    }

    fn range(min_x: u32, min_y: u32, max_x: u32, max_y: u32) -> TileRange {
        TileRange {
            zoom: 3,
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    #[test]
    fn tile_range_len_and_emptiness() {
        assert_eq!(range(2, 2, 2, 2).len(), 1);
        assert!(!range(2, 2, 2, 2).is_empty());
        assert_eq!(range(0, 0, 7, 7).len(), 64);
        for inverted in [range(3, 0, 2, 7), range(0, 3, 7, 2), range(5, 5, 0, 0)] {
            assert!(inverted.is_empty(), "{inverted:?}");
            assert_eq!(inverted.len(), 0, "{inverted:?}");
            assert!(!inverted.contains(2, 2) && !inverted.contains(3, 3));
        }
        let full = range(0, 0, u32::MAX, u32::MAX);
        assert_eq!(full.len(), u64::MAX, "saturates instead of overflowing");
        assert_eq!(range(0, 0, u32::MAX, 0).len(), 1 << 32);
    }

    #[test]
    fn bbox_range_orders_rows_southward() {
        let r = bbox_to_tile_range(&BBox::new(-10.0, -10.0, 10.0, 10.0), 2);
        assert_eq!((r.min_x, r.min_y, r.max_x, r.max_y), (1, 1, 2, 2));
        assert_eq!(r.len(), 4);
    }
}
