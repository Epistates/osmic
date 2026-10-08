use serde::{Deserialize, Serialize};

/// Fixed-point scale used by OSM: coordinates are stored in units of 1e-7 degrees.
pub const COORDINATE_SCALE: f64 = 1e7;

/// A geographic coordinate in longitude/latitude (WGS84).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LonLat {
    pub lon: f64,
    pub lat: f64,
}

impl LonLat {
    pub const fn new(lon: f64, lat: f64) -> Self {
        Self { lon, lat }
    }

    /// Returns true if this coordinate is within valid WGS84 bounds.
    pub fn is_valid(&self) -> bool {
        (-180.0..=180.0).contains(&self.lon) && (-90.0..=90.0).contains(&self.lat)
    }
}

impl From<LonLat> for geo_types::Coord<f64> {
    fn from(ll: LonLat) -> Self {
        geo_types::Coord {
            x: ll.lon,
            y: ll.lat,
        }
    }
}

impl From<geo_types::Coord<f64>> for LonLat {
    fn from(c: geo_types::Coord<f64>) -> Self {
        Self { lon: c.x, lat: c.y }
    }
}

/// A coordinate in OSM's native fixed-point representation: integer units of
/// 1e-7 degrees (about 1.1 cm at the equator).
///
/// This is exactly the precision OSM stores and PBF files carry (with the
/// default granularity), so round-tripping through `FixedCoord` is lossless
/// for OSM data. Every valid coordinate fits in an `i32`:
/// ±180° = ±1_800_000_000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FixedCoord {
    /// Longitude in 1e-7 degrees.
    pub lon: i32,
    /// Latitude in 1e-7 degrees.
    pub lat: i32,
}

/// Flips the sign bit so that `i32::MIN` (never a valid coordinate) maps to 0.
const SIGN_FLIP: u32 = 0x8000_0000;

impl FixedCoord {
    /// Construct from raw 1e-7 degree units.
    pub const fn new(lon: i32, lat: i32) -> Self {
        Self { lon, lat }
    }

    /// Convert from floating-point degrees, rounding to the nearest 1e-7.
    ///
    /// Returns `None` for non-finite input or coordinates outside
    /// [-180, 180] × [-90, 90].
    pub fn from_degrees(lon: f64, lat: f64) -> Option<Self> {
        let ll = LonLat::new(lon, lat);
        if !lon.is_finite() || !lat.is_finite() || !ll.is_valid() {
            return None;
        }
        // In range by the check above, so the casts cannot saturate.
        Some(Self {
            lon: (lon * COORDINATE_SCALE).round() as i32,
            lat: (lat * COORDINATE_SCALE).round() as i32,
        })
    }

    /// Longitude in degrees.
    pub fn lon_degrees(self) -> f64 {
        f64::from(self.lon) / COORDINATE_SCALE
    }

    /// Latitude in degrees.
    pub fn lat_degrees(self) -> f64 {
        f64::from(self.lat) / COORDINATE_SCALE
    }

    /// Convert to floating-point degrees.
    pub fn to_lonlat(self) -> LonLat {
        LonLat::new(self.lon_degrees(), self.lat_degrees())
    }

    /// Convert to a `geo_types` coordinate (x = lon, y = lat, in degrees).
    pub fn to_coord(self) -> geo_types::Coord<f64> {
        geo_types::Coord {
            x: self.lon_degrees(),
            y: self.lat_degrees(),
        }
    }

    /// Returns true if the coordinate is within WGS84 bounds.
    pub const fn is_valid(self) -> bool {
        self.lon >= -1_800_000_000
            && self.lon <= 1_800_000_000
            && self.lat >= -900_000_000
            && self.lat <= 900_000_000
    }

    /// Pack into a `u64` whose value is never 0 for a valid coordinate.
    ///
    /// Zero is reserved as the "empty slot" marker so zero-filled memory
    /// (fresh mmap pages) reads as "no node here".
    pub const fn pack(self) -> u64 {
        let lon = (self.lon as u32) ^ SIGN_FLIP;
        let lat = (self.lat as u32) ^ SIGN_FLIP;
        ((lon as u64) << 32) | lat as u64
    }

    /// Inverse of [`FixedCoord::pack`]. Returns `None` for the empty marker 0.
    pub const fn unpack(packed: u64) -> Option<Self> {
        if packed == 0 {
            return None;
        }
        let lon = ((packed >> 32) as u32 ^ SIGN_FLIP) as i32;
        let lat = (packed as u32 ^ SIGN_FLIP) as i32;
        Some(Self { lon, lat })
    }
}

impl From<FixedCoord> for LonLat {
    fn from(c: FixedCoord) -> Self {
        c.to_lonlat()
    }
}

impl From<FixedCoord> for geo_types::Coord<f64> {
    fn from(c: FixedCoord) -> Self {
        c.to_coord()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lonlat_valid_within_bounds() {
        assert!(LonLat::new(0.0, 0.0).is_valid());
        assert!(LonLat::new(180.0, 90.0).is_valid());
        assert!(LonLat::new(-180.0, -90.0).is_valid());
        assert!(LonLat::new(179.999, 89.999).is_valid());
    }

    #[test]
    fn lonlat_invalid_outside_bounds() {
        assert!(!LonLat::new(180.001, 0.0).is_valid());
        assert!(!LonLat::new(-180.001, 0.0).is_valid());
        assert!(!LonLat::new(0.0, 90.001).is_valid());
        assert!(!LonLat::new(0.0, -90.001).is_valid());
    }

    #[test]
    fn fixed_round_trip_is_exact_at_osm_precision() {
        // Values with exactly 7 decimals must survive degrees → fixed → degrees.
        for &(lon, lat) in &[
            (-122.419_415_5, 37.774_929_5),
            (179.999_999_9, -89.999_999_9),
            (0.000_000_1, -0.000_000_1),
            (13.404_954, 52.520_007),
        ] {
            let f = FixedCoord::from_degrees(lon, lat).expect("valid");
            assert_eq!(
                FixedCoord::from_degrees(f.lon_degrees(), f.lat_degrees()),
                Some(f)
            );
            assert!((f.lon_degrees() - lon).abs() < 0.5e-7, "lon {lon}");
            assert!((f.lat_degrees() - lat).abs() < 0.5e-7, "lat {lat}");
        }
    }

    #[test]
    fn fixed_rejects_invalid_input() {
        assert!(FixedCoord::from_degrees(f64::NAN, 0.0).is_none());
        assert!(FixedCoord::from_degrees(0.0, f64::INFINITY).is_none());
        assert!(FixedCoord::from_degrees(180.000_001, 0.0).is_none());
        assert!(FixedCoord::from_degrees(0.0, -90.000_001).is_none());
    }

    #[test]
    fn pack_round_trip_and_zero_is_reserved() {
        for &(lon, lat) in &[
            (0, 0),
            (-1_800_000_000, -900_000_000),
            (1_800_000_000, 900_000_000),
            (-1, 1),
            (123_456_789, -98_765_432),
        ] {
            let c = FixedCoord::new(lon, lat);
            let packed = c.pack();
            assert_ne!(packed, 0, "valid coordinate packed to the empty marker");
            assert_eq!(FixedCoord::unpack(packed), Some(c));
        }
        assert_eq!(FixedCoord::unpack(0), None);
    }

    #[test]
    fn packed_order_is_independent_of_validity_marker() {
        // (0,0) — the Gulf of Guinea — is a real location, not "empty".
        let origin = FixedCoord::new(0, 0);
        assert_eq!(FixedCoord::unpack(origin.pack()), Some(origin));
    }
}
