//! Polygon winding-order correction.
//!
//! Enforces OGC Simple Features / GeoJSON RFC 7946 convention in geographic
//! (lon/lat) space:
//!
//! - **Exterior rings**: counter-clockwise
//! - **Interior rings (holes)**: clockwise
//!
//! The downstream MVT encoder projects lon/lat to tile-local coordinates,
//! which flips the Y axis. That projection converts CCW lon/lat to CW
//! tile-local, matching the MVT v2.1 spec which requires CW outer rings in
//! tile coordinates. So getting this right here produces correct MVT output.
//!
//! We use `geo::algorithm::winding_order::Winding` for the underlying
//! orientation test and reversal — it's well-tested and handles degenerate
//! cases (fewer than 4 coords, unclosed rings) gracefully.

use geo::algorithm::winding_order::Winding;
use geo_types::{LineString, MultiPolygon, Polygon};
use osmic_core::geometry::Geometry as OsmicGeometry;

/// Enforce CCW exterior / CW interior winding on a single polygon.
///
/// No-op for rings that already have the correct orientation. Rings with
/// fewer than 4 distinct coordinates have an unspecified winding order per
/// `geo::Winding::winding_order` and are left untouched.
pub fn orient_polygon(poly: &mut Polygon<f64>) {
    // Exterior: ensure CCW.
    poly.exterior_mut(|ls: &mut LineString<f64>| {
        if ls.is_cw() {
            ls.make_ccw_winding();
        }
    });

    // Interiors: ensure CW.
    poly.interiors_mut(|interiors: &mut [LineString<f64>]| {
        for ring in interiors {
            if ring.is_ccw() {
                ring.make_cw_winding();
            }
        }
    });
}

/// Enforce winding on every polygon in a `MultiPolygon`.
pub fn orient_multipolygon(mp: &mut MultiPolygon<f64>) {
    for poly in mp.0.iter_mut() {
        orient_polygon(poly);
    }
}

/// Convenience: enforce winding on any `osmic_core::Geometry` variant that
/// contains polygons. Other variants are left untouched.
pub fn orient_geometry(geom: &mut OsmicGeometry) {
    match geom {
        OsmicGeometry::Polygon(p) => orient_polygon(p),
        OsmicGeometry::MultiPolygon(mp) => orient_multipolygon(mp),
        // Points and lines don't have a meaningful winding to correct.
        OsmicGeometry::Point(_)
        | OsmicGeometry::MultiPoint(_)
        | OsmicGeometry::Line(_)
        | OsmicGeometry::MultiLine(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{LineString, Polygon, coord};

    /// A CW exterior should be flipped to CCW.
    #[test]
    fn flip_cw_exterior_to_ccw() {
        // Clockwise unit square in lon/lat space.
        let cw = LineString::from(vec![
            (0.0, 0.0),
            (0.0, 1.0),
            (1.0, 1.0),
            (1.0, 0.0),
            (0.0, 0.0),
        ]);
        assert!(cw.is_cw(), "test data sanity: input must be CW");

        let mut poly = Polygon::new(cw, vec![]);
        orient_polygon(&mut poly);
        assert!(
            poly.exterior().is_ccw(),
            "orient_polygon must flip CW exterior to CCW"
        );
    }

    /// A CCW exterior should be left alone.
    #[test]
    fn ccw_exterior_unchanged() {
        let ccw = LineString::from(vec![
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.0, 1.0),
            (0.0, 0.0),
        ]);
        assert!(ccw.is_ccw(), "test data sanity: input must be CCW");

        let original = ccw.clone();
        let mut poly = Polygon::new(ccw, vec![]);
        orient_polygon(&mut poly);
        assert_eq!(
            poly.exterior(),
            &original,
            "CCW exterior should be left untouched"
        );
    }

    /// A CCW interior should be flipped to CW (interiors must be clockwise).
    #[test]
    fn flip_ccw_interior_to_cw() {
        let exterior = LineString::from(vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ]);
        let ccw_hole = LineString::from(vec![
            (2.0, 2.0),
            (3.0, 2.0),
            (3.0, 3.0),
            (2.0, 3.0),
            (2.0, 2.0),
        ]);
        assert!(ccw_hole.is_ccw(), "test data sanity: hole must start CCW");

        let mut poly = Polygon::new(exterior, vec![ccw_hole]);
        orient_polygon(&mut poly);
        assert!(
            poly.interiors()[0].is_cw(),
            "orient_polygon must flip CCW interior to CW"
        );
    }

    /// MultiPolygon applies to every polygon.
    #[test]
    fn orient_multipolygon_fixes_each_polygon() {
        let cw1 = Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (0.0, 1.0),
                (1.0, 1.0),
                (1.0, 0.0),
                (0.0, 0.0),
            ]),
            vec![],
        );
        let cw2 = Polygon::new(
            LineString::from(vec![
                (5.0, 5.0),
                (5.0, 6.0),
                (6.0, 6.0),
                (6.0, 5.0),
                (5.0, 5.0),
            ]),
            vec![],
        );
        let mut mp = MultiPolygon::new(vec![cw1, cw2]);
        orient_multipolygon(&mut mp);
        for poly in &mp.0 {
            assert!(
                poly.exterior().is_ccw(),
                "each polygon's exterior must end up CCW"
            );
        }
    }

    /// A degenerate ring with <4 coords has no defined winding; orient_polygon
    /// must not panic.
    #[test]
    fn orient_polygon_degenerate_is_noop() {
        let degenerate =
            LineString::from(vec![coord! { x: 0.0, y: 0.0 }, coord! { x: 1.0, y: 1.0 }]);
        let mut poly = Polygon::new(degenerate, vec![]);
        // Must not panic. We don't assert anything about the result — the
        // winding order is undefined for <4 coords.
        orient_polygon(&mut poly);
    }

    /// orient_geometry dispatches correctly across osmic_core::Geometry variants.
    #[test]
    fn orient_geometry_dispatches() {
        use osmic_core::geometry::Geometry as OsmicGeometry;

        // Polygon case — CW input must flip to CCW
        let cw = LineString::from(vec![
            (0.0, 0.0),
            (0.0, 1.0),
            (1.0, 1.0),
            (1.0, 0.0),
            (0.0, 0.0),
        ]);
        let mut g = OsmicGeometry::Polygon(Polygon::new(cw, vec![]));
        orient_geometry(&mut g);
        if let OsmicGeometry::Polygon(p) = g {
            assert!(p.exterior().is_ccw(), "polygon should be flipped to CCW");
        } else {
            panic!("expected polygon variant");
        }

        // Point case — must not panic, no-op
        let mut p = OsmicGeometry::Point(geo_types::Point::new(0.0, 0.0));
        orient_geometry(&mut p);

        // Line case — must not panic, no-op
        let mut l = OsmicGeometry::Line(LineString::from(vec![(0.0, 0.0), (1.0, 1.0)]));
        orient_geometry(&mut l);
    }
}
