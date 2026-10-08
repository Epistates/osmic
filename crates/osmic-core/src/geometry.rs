//! Feature geometry.

use geo_types::{Coord, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};

use crate::bbox::BBox;

/// Unified geometry enum for OSM features.
///
/// Coordinates are `x` = longitude, `y` = latitude in degrees for
/// geographic geometry; the same type also carries projected (tile-space)
/// geometry, where they are planar x and y.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    /// A single point, such as a tagged node.
    Point(Point<f64>),
    /// Several points treated as one feature.
    MultiPoint(MultiPoint<f64>),
    /// An open or closed polyline, such as a way that is not an area.
    Line(LineString<f64>),
    /// Several polylines, such as a route relation or a clipped line.
    MultiLine(MultiLineString<f64>),
    /// An exterior ring with optional holes.
    Polygon(Polygon<f64>),
    /// Several polygons, such as a multipolygon relation.
    MultiPolygon(MultiPolygon<f64>),
}

/// Coarse geometry class, matching the three MVT geometry types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeometryType {
    /// [`Geometry::Point`] or [`Geometry::MultiPoint`].
    Point,
    /// [`Geometry::Line`] or [`Geometry::MultiLine`].
    Line,
    /// [`Geometry::Polygon`] or [`Geometry::MultiPolygon`].
    Polygon,
}

impl Geometry {
    /// The MVT-level class of this geometry (multi-variants map to their
    /// single counterpart).
    pub fn geometry_type(&self) -> GeometryType {
        match self {
            Self::Point(_) | Self::MultiPoint(_) => GeometryType::Point,
            Self::Line(_) | Self::MultiLine(_) => GeometryType::Line,
            Self::Polygon(_) | Self::MultiPolygon(_) => GeometryType::Polygon,
        }
    }

    /// Visit every coordinate (including polygon holes).
    pub fn for_each_coord(&self, mut f: impl FnMut(Coord<f64>)) {
        match self {
            Self::Point(p) => f(p.0),
            Self::MultiPoint(mp) => mp.0.iter().for_each(|p| f(p.0)),
            Self::Line(ls) => ls.0.iter().for_each(|c| f(*c)),
            Self::MultiLine(mls) => mls.0.iter().flat_map(|l| &l.0).for_each(|c| f(*c)),
            Self::Polygon(poly) => polygon_coords(poly).for_each(|c| f(*c)),
            Self::MultiPolygon(mp) => mp.0.iter().flat_map(polygon_coords).for_each(|c| f(*c)),
        }
    }

    /// Total number of coordinates.
    pub fn coord_count(&self) -> usize {
        let mut n = 0;
        self.for_each_coord(|_| n += 1);
        n
    }

    /// Axis-aligned bounding box of this geometry. Holes lie inside their
    /// exterior, so only the outer shape determines the result, but every
    /// coordinate is considered so malformed input still yields a box that
    /// contains all of it.
    pub fn bbox(&self) -> BBox {
        let mut bb = BBox::empty();
        self.for_each_coord(|c| bb.expand(c.x, c.y));
        bb
    }

    /// True if the geometry has no coordinates.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Point(_) => false,
            Self::MultiPoint(mp) => mp.0.is_empty(),
            Self::Line(ls) => ls.0.is_empty(),
            Self::MultiLine(mls) => mls.0.iter().all(|l| l.0.is_empty()),
            Self::Polygon(p) => p.exterior().0.is_empty(),
            Self::MultiPolygon(mp) => mp.0.iter().all(|p| p.exterior().0.is_empty()),
        }
    }
}

fn polygon_coords(poly: &Polygon<f64>) -> impl Iterator<Item = &Coord<f64>> {
    poly.exterior()
        .0
        .iter()
        .chain(poly.interiors().iter().flat_map(|r| &r.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_covers_every_part() {
        let g = Geometry::MultiLine(MultiLineString(vec![
            LineString::from(vec![(0.0, 0.0), (1.0, 1.0)]),
            LineString::from(vec![(5.0, -2.0), (6.0, 3.0)]),
        ]));
        assert_eq!(g.bbox(), BBox::new(0.0, -2.0, 6.0, 3.0));
        assert_eq!(g.coord_count(), 4);
        assert_eq!(g.geometry_type(), GeometryType::Line);
    }

    #[test]
    fn multipoint_bbox() {
        let g = Geometry::MultiPoint(MultiPoint(vec![
            Point::new(1.0, 2.0),
            Point::new(-1.0, 4.0),
        ]));
        assert_eq!(g.bbox(), BBox::new(-1.0, 2.0, 1.0, 4.0));
        assert_eq!(g.geometry_type(), GeometryType::Point);
    }
}
