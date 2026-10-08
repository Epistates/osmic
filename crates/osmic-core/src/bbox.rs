//! Axis-aligned bounding boxes.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::coord::LonLat;

/// Axis-aligned bounding box in geographic coordinates (WGS84).
///
/// Bounds are inclusive. Boxes crossing the antimeridian are not
/// represented: `min_lon > max_lon` means the box is empty (see
/// [`BBox::is_valid`]), not that it wraps. In projected spaces (see
/// [`crate::clip`]) the `lon` fields read as x and the `lat` fields as y.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    /// Western edge, in degrees.
    pub min_lon: f64,
    /// Southern edge, in degrees.
    pub min_lat: f64,
    /// Eastern edge, in degrees.
    pub max_lon: f64,
    /// Northern edge, in degrees.
    pub max_lat: f64,
}

impl BBox {
    /// A box from its edges, in `(west, south, east, north)` order. Not
    /// validated.
    pub const fn new(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> Self {
        Self {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        }
    }

    /// An empty bbox that can be extended with `expand`.
    pub const fn empty() -> Self {
        Self {
            min_lon: f64::MAX,
            min_lat: f64::MAX,
            max_lon: f64::MIN,
            max_lat: f64::MIN,
        }
    }

    /// The full world extent.
    pub const fn world() -> Self {
        Self {
            min_lon: -180.0,
            min_lat: -85.051_129,
            max_lon: 180.0,
            max_lat: 85.051_129,
        }
    }

    /// Expand this bbox to include the given point.
    pub fn expand(&mut self, lon: f64, lat: f64) {
        self.min_lon = self.min_lon.min(lon);
        self.min_lat = self.min_lat.min(lat);
        self.max_lon = self.max_lon.max(lon);
        self.max_lat = self.max_lat.max(lat);
    }

    /// Merge another bbox into this one.
    pub fn extend(&mut self, other: &BBox) {
        self.min_lon = self.min_lon.min(other.min_lon);
        self.min_lat = self.min_lat.min(other.min_lat);
        self.max_lon = self.max_lon.max(other.max_lon);
        self.max_lat = self.max_lat.max(other.max_lat);
    }

    /// Whether the point lies inside the box or on its boundary.
    pub fn contains_point(&self, lon: f64, lat: f64) -> bool {
        lon >= self.min_lon && lon <= self.max_lon && lat >= self.min_lat && lat <= self.max_lat
    }

    /// Whether the two boxes overlap; boxes that only touch along an edge or
    /// at a corner count as intersecting.
    pub fn intersects(&self, other: &BBox) -> bool {
        self.min_lon <= other.max_lon
            && self.max_lon >= other.min_lon
            && self.min_lat <= other.max_lat
            && self.max_lat >= other.min_lat
    }

    /// Midpoint of the box, in degrees (meaningless for an empty box).
    pub fn center(&self) -> LonLat {
        LonLat::new(
            (self.min_lon + self.max_lon) / 2.0,
            (self.min_lat + self.max_lat) / 2.0,
        )
    }

    /// East-west extent, in degrees of longitude. Negative for an empty box.
    pub fn width(&self) -> f64 {
        self.max_lon - self.min_lon
    }

    /// North-south extent, in degrees of latitude. Negative for an empty box.
    pub fn height(&self) -> f64 {
        self.max_lat - self.min_lat
    }

    /// Returns true if this bbox has been expanded at least once.
    pub fn is_valid(&self) -> bool {
        self.min_lon <= self.max_lon && self.min_lat <= self.max_lat
    }
}

impl Default for BBox {
    /// The empty box (see [`BBox::empty`]), ready to be expanded.
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Display for BBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{:.6}, {:.6}, {:.6}, {:.6}]",
            self.min_lon, self.min_lat, self.max_lon, self.max_lat
        )
    }
}

impl From<BBox> for geo_types::Rect<f64> {
    fn from(bbox: BBox) -> Self {
        geo_types::Rect::new(
            geo_types::Coord {
                x: bbox.min_lon,
                y: bbox.min_lat,
            },
            geo_types::Coord {
                x: bbox.max_lon,
                y: bbox.max_lat,
            },
        )
    }
}

impl From<geo_types::Rect<f64>> for BBox {
    fn from(r: geo_types::Rect<f64>) -> Self {
        Self {
            min_lon: r.min().x,
            min_lat: r.min().y,
            max_lon: r.max().x,
            max_lat: r.max().y,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_not_valid() {
        assert!(!BBox::empty().is_valid());
    }

    #[test]
    fn expand_once_makes_valid() {
        let mut b = BBox::empty();
        b.expand(10.0, 20.0);
        assert!(b.is_valid());
        assert_eq!(b.min_lon, 10.0);
        assert_eq!(b.min_lat, 20.0);
        assert_eq!(b.max_lon, 10.0);
        assert_eq!(b.max_lat, 20.0);
    }

    #[test]
    fn contains_point_boundary_cases() {
        let b = BBox::new(0.0, 0.0, 10.0, 10.0);
        // Corners are inside (inclusive).
        assert!(b.contains_point(0.0, 0.0));
        assert!(b.contains_point(10.0, 10.0));
        assert!(b.contains_point(0.0, 10.0));
        assert!(b.contains_point(10.0, 0.0));
        // Edge midpoints are inside.
        assert!(b.contains_point(5.0, 0.0));
        assert!(b.contains_point(0.0, 5.0));
        // Just outside is not inside.
        assert!(!b.contains_point(-0.001, 5.0));
        assert!(!b.contains_point(5.0, 10.001));
    }

    #[test]
    fn intersects_is_symmetric() {
        let a = BBox::new(0.0, 0.0, 5.0, 5.0);
        let b = BBox::new(3.0, 3.0, 8.0, 8.0);
        assert!(a.intersects(&b));
        assert!(b.intersects(&a));
    }

    #[test]
    fn non_overlapping_does_not_intersect() {
        let a = BBox::new(0.0, 0.0, 5.0, 5.0);
        let b = BBox::new(6.0, 0.0, 10.0, 5.0);
        assert!(!a.intersects(&b));
        assert!(!b.intersects(&a));
    }

    #[test]
    fn extend_with_empty_does_not_shrink() {
        let original = BBox::new(1.0, 2.0, 3.0, 4.0);
        let mut b = original;
        b.extend(&BBox::empty());
        // Extending with empty (which has MAX/MIN sentinels) should not change a
        // valid bbox, because min(valid, f64::MAX) == valid and
        // max(valid, f64::MIN) == valid.
        assert_eq!(b, original);
    }

    #[test]
    fn extend_merges_two_bboxes() {
        let mut a = BBox::new(0.0, 0.0, 5.0, 5.0);
        let b = BBox::new(3.0, 3.0, 10.0, 10.0);
        a.extend(&b);
        assert_eq!(a, BBox::new(0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn center_correctness() {
        let b = BBox::new(0.0, 0.0, 10.0, 6.0);
        let c = b.center();
        assert!((c.lon - 5.0).abs() < 1e-12);
        assert!((c.lat - 3.0).abs() < 1e-12);
    }

    #[test]
    fn width_and_height_correctness() {
        let b = BBox::new(1.0, 2.0, 4.0, 9.0);
        assert!((b.width() - 3.0).abs() < 1e-12);
        assert!((b.height() - 7.0).abs() < 1e-12);
    }
}
