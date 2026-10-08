//! Backend-independent semantics of the public CPU API (also the reference the
//! GPU is compared against in `gpu_parity.rs`).

mod common;

use geo_types::{Coord, LineString, MultiPolygon, Polygon};
use osmic_accel::{clip_batch_cpu, ClipOptions, ClippedGeometry, WorkItem};
use osmic_core::geometry::Geometry;

use common::{frame, TileFrame, EXTENT, TILE_X, TILE_Y, ZOOM};

fn item(geometry: &Geometry) -> WorkItem<'_> {
    WorkItem {
        geometry,
        tile_x: TILE_X,
        tile_y: TILE_Y,
        zoom: ZOOM,
        extent: EXTENT,
    }
}

fn rect(f: &TileFrame, cx: f64, cy: f64, w: f64, h: f64) -> LineString<f64> {
    let (x0, x1) = (f.cx + (cx - w) * f.hw, f.cx + (cx + w) * f.hw);
    let (y0, y1) = (f.cy + (cy - h) * f.hh, f.cy + (cy + h) * f.hh);
    LineString(
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]
            .map(|(x, y)| Coord { x, y })
            .to_vec(),
    )
}

fn clip_one(geometry: &Geometry) -> ClippedGeometry {
    clip_batch_cpu(&[item(geometry)], &ClipOptions::default())
        .unwrap()
        .remove(0)
}

fn shoelace(r: &[[f32; 2]]) -> f64 {
    let mut a = 0.0f64;
    for i in 0..r.len() {
        let (p, q) = (r[i], r[(i + 1) % r.len()]);
        a += f64::from(p[0]) * f64::from(q[1]) - f64::from(q[0]) * f64::from(p[1]);
    }
    a / 2.0
}

#[test]
fn multipolygon_keeps_every_polygon() {
    let f = frame();
    let a = Polygon::new(rect(&f, -0.5, 0.0, 0.2, 0.2), vec![]);
    let b = Polygon::new(rect(&f, 0.5, 0.0, 0.2, 0.2), vec![]);
    let out = clip_one(&Geometry::MultiPolygon(MultiPolygon(vec![a, b])));
    let ClippedGeometry::Polygons(polys) = out else {
        panic!("expected polygons, got {out:?}");
    };
    assert_eq!(polys.len(), 2);
}

#[test]
fn holes_are_preserved() {
    let f = frame();
    let poly = Polygon::new(
        rect(&f, 0.0, 0.0, 0.8, 0.8),
        vec![rect(&f, 0.0, 0.0, 0.2, 0.2)],
    );
    let ClippedGeometry::Polygons(polys) = clip_one(&Geometry::Polygon(poly)) else {
        panic!("expected polygon");
    };
    assert_eq!(polys.len(), 1);
    assert_eq!(polys[0].holes.len(), 1);
    // Areas are in tile units; 0.8 and 0.2 half-extents of a 4096 tile.
    let outer = shoelace(&polys[0].exterior).abs();
    let hole = shoelace(&polys[0].holes[0]).abs();
    let e = f64::from(EXTENT);
    let want_outer = (0.8 * e) * (0.8 * e);
    let want_hole = (0.2 * e) * (0.2 * e);
    // Mercator is not linear in latitude, so allow a few percent.
    assert!((outer / want_outer - 1.0).abs() < 0.05, "outer {outer}");
    assert!((hole / want_hole - 1.0).abs() < 0.05, "hole {hole}");
}

#[test]
fn hole_outside_tile_is_dropped_and_exterior_clipped() {
    let f = frame();
    // Exterior spans past the east edge; the hole lies entirely outside.
    let poly = Polygon::new(
        rect(&f, 0.8, 0.0, 1.2, 0.5),
        vec![rect(&f, 1.8, 0.0, 0.1, 0.1)],
    );
    let ClippedGeometry::Polygons(polys) = clip_one(&Geometry::Polygon(poly)) else {
        panic!("expected polygon");
    };
    assert_eq!(polys.len(), 1);
    assert!(polys[0].holes.is_empty());
}

#[test]
fn line_reentering_tile_becomes_separate_parts() {
    let f = frame();
    let p = |x: f64, y: f64| Coord {
        x: f.cx + x * f.hw,
        y: f.cy + y * f.hh,
    };
    // In, out the east side, back in, out the north side, back in.
    let line = LineString(vec![
        p(-0.5, 0.0),
        p(2.0, 0.0),
        p(2.0, 0.4),
        p(0.0, 0.4),
        p(0.0, 3.0),
        p(0.5, 3.0),
        p(0.5, 0.2),
    ]);
    let ClippedGeometry::Lines(parts) = clip_one(&Geometry::Line(line)) else {
        panic!("expected lines");
    };
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(parts.iter().all(|p| p.len() >= 2));
    // No part may contain a jump across the tile: consecutive vertices of
    // a part must be near the original path, so check all stay in the box.
    let hi = EXTENT as f32 * 1.05 + 1e-3;
    let lo = -(EXTENT as f32) * 0.05 - 1e-3;
    for part in &parts {
        for v in part {
            assert!(
                v[0] >= lo && v[0] <= hi && v[1] >= lo && v[1] <= hi,
                "{v:?}"
            );
        }
    }
}

#[test]
fn polygon_larger_than_old_vertex_limit_is_not_truncated() {
    // A 5000-vertex circle entirely inside the tile must keep all vertices.
    let f = frame();
    let n = 5000;
    let mut coords: Vec<Coord<f64>> = (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            Coord {
                x: f.cx + 0.5 * a.cos() * f.hw,
                y: f.cy + 0.5 * a.sin() * f.hh,
            }
        })
        .collect();
    coords.push(coords[0]);
    let poly = Polygon::new(LineString(coords), vec![]);
    let ClippedGeometry::Polygons(polys) = clip_one(&Geometry::Polygon(poly)) else {
        panic!("expected polygon");
    };
    assert_eq!(polys[0].exterior.len(), n);
}

#[test]
fn invalid_input_is_an_error_not_a_panic() {
    let nan = Geometry::Point(geo_types::Point::new(f64::NAN, 0.0));
    let err = clip_batch_cpu(&[item(&nan)], &ClipOptions::default()).unwrap_err();
    assert!(
        matches!(err, osmic_accel::AccelError::InvalidInput(_)),
        "{err}"
    );

    let ok = Geometry::Point(geo_types::Point::new(0.0, 0.0));
    let bad_zoom = WorkItem {
        zoom: 99,
        ..item(&ok)
    };
    assert!(clip_batch_cpu(&[bad_zoom], &ClipOptions::default()).is_err());

    let mut opts = ClipOptions::default();
    opts.ring_capacity_factor = 0;
    assert!(clip_batch_cpu(&[], &opts).is_err());
}

#[test]
fn results_follow_input_order() {
    let f = frame();
    let inside = Geometry::Point(geo_types::Point::new(f.cx, f.cy));
    let outside = Geometry::Point(geo_types::Point::new(f.cx + 10.0 * f.hw, f.cy));
    let out = clip_batch_cpu(
        &[item(&inside), item(&outside), item(&inside)],
        &ClipOptions::default(),
    )
    .unwrap();
    assert!(matches!(out[0], ClippedGeometry::Point(_)));
    assert!(out[1].is_empty());
    assert!(matches!(out[2], ClippedGeometry::Point(_)));
}
