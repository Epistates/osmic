//! Ramer–Douglas–Peucker simplification.
//!
//! The tolerance is in the coordinate units of the input. Callers that
//! simplify for display should project first and pass a tolerance in
//! pixels (the tile renderer does this per zoom level); simplifying raw
//! degrees distorts shapes away from the equator.

use geo_types::{LineString, MultiLineString, MultiPolygon, Polygon};
use osmic_core::Geometry;
use osmic_core::simplify::rdp;

/// Simplify a line string.
pub fn simplify_line(line: &LineString<f64>, tolerance: f64) -> LineString<f64> {
    let mut out = Vec::with_capacity(line.0.len());
    rdp(&line.0, tolerance, &mut out);
    LineString(out)
}

/// Simplify a polygon's exterior and holes. Rings that collapse below four
/// coordinates are dropped (a collapsed exterior yields `None`).
pub fn simplify_polygon(poly: &Polygon<f64>, tolerance: f64) -> Option<Polygon<f64>> {
    let exterior = simplify_line(poly.exterior(), tolerance);
    if exterior.0.len() < 4 {
        return None;
    }
    let interiors = poly
        .interiors()
        .iter()
        .map(|r| simplify_line(r, tolerance))
        .filter(|r| r.0.len() >= 4)
        .collect();
    Some(Polygon::new(exterior, interiors))
}

/// Simplify any geometry. Returns `None` if nothing meaningful remains
/// (every polygon collapsed or every line shrank to a single point).
pub fn simplify_geometry(geom: &Geometry, tolerance: f64) -> Option<Geometry> {
    let keep_line = |l: LineString<f64>| (l.0.len() >= 2).then_some(l);
    match geom {
        Geometry::Point(_) | Geometry::MultiPoint(_) => Some(geom.clone()),
        Geometry::Line(ls) => keep_line(simplify_line(ls, tolerance)).map(Geometry::Line),
        Geometry::MultiLine(mls) => {
            let parts: Vec<_> = mls
                .0
                .iter()
                .filter_map(|l| keep_line(simplify_line(l, tolerance)))
                .collect();
            (!parts.is_empty()).then_some(Geometry::MultiLine(MultiLineString(parts)))
        }
        Geometry::Polygon(p) => simplify_polygon(p, tolerance).map(Geometry::Polygon),
        Geometry::MultiPolygon(mp) => {
            let polys: Vec<_> =
                mp.0.iter()
                    .filter_map(|p| simplify_polygon(p, tolerance))
                    .collect();
            (!polys.is_empty()).then_some(Geometry::MultiPolygon(MultiPolygon(polys)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collinear_points_are_removed() {
        let l = LineString::from(vec![(0.0, 0.0), (1.0, 0.0001), (2.0, 0.0)]);
        assert_eq!(simplify_line(&l, 0.01).0.len(), 2);
    }

    #[test]
    fn tiny_polygon_collapses_to_none() {
        let p = Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (0.1, 0.0),
                (0.1, 0.1),
                (0.0, 0.1),
                (0.0, 0.0),
            ]),
            vec![],
        );
        assert!(simplify_polygon(&p, 10.0).is_none());
        assert!(simplify_polygon(&p, 0.001).is_some());
    }
}
