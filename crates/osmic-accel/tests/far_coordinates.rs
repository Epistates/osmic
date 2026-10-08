//! Geometry reaching far beyond the tile: huge but finite coordinates and
//! edges spanning thousands of tiles. Checked on the CPU and, when Metal is
//! available, on the GPU, against results computed in f64.

mod common;

use geo_types::{Coord, LineString, Polygon};
use osmic_accel::{AccelError, ClippedGeometry, WorkItem};
use osmic_core::geometry::Geometry;
use osmic_core::mercator::{unit_x_to_lon, unit_y_to_lat};

use common::on_every_backend;

/// Clip-box edges for extent 4096 with the default 5% buffer.
const LO: f64 = -204.8;
const HI: f64 = 4300.8;

fn z0(geometry: &Geometry) -> WorkItem<'_> {
    WorkItem {
        geometry,
        tile_x: 0,
        tile_y: 0,
        zoom: 0,
        extent: 4096,
    }
}

fn coord(lon: f64, lat: f64) -> Coord<f64> {
    Coord { x: lon, y: lat }
}

/// Tile-local position of `(lon, lat)` in tile 0/0/0, in f64.
fn local_z0(lon: f64, lat: f64) -> [f64; 2] {
    [
        osmic_core::mercator::lon_to_unit_x(lon) * 4096.0,
        osmic_core::mercator::lat_to_unit_y(lat) * 4096.0,
    ]
}

fn all_finite(points: &[[f32; 2]]) -> bool {
    points.iter().flatten().all(|v| v.is_finite())
}

fn shoelace(r: &[[f32; 2]]) -> f64 {
    let mut a = 0.0f64;
    for i in 0..r.len() {
        let (p, q) = (r[i], r[(i + 1) % r.len()]);
        a += f64::from(p[0]) * f64::from(q[1]) - f64::from(q[0]) * f64::from(p[1]);
    }
    a / 2.0
}

fn near(got: [f32; 2], want: [f64; 2], tol: f64) -> bool {
    (f64::from(got[0]) - want[0]).abs() <= tol && (f64::from(got[1]) - want[1]).abs() <= tol
}

#[test]
fn a_huge_longitude_does_not_turn_a_polygon_into_nan() {
    // lon 1e300 is finite in f64 but overflows f32.
    let poly = Geometry::Polygon(Polygon::new(
        LineString(vec![coord(-1.0, -1.0), coord(1e300, -1.0), coord(1.0, 1.0)]),
        vec![],
    ));
    // Inside the clip box the triangle is a trapezoid: the edge towards
    // lon 1e300 is the horizontal line at lat -1, the closing edge from it
    // to (1, 1) is horizontal at lat 1 to within 1e-297.
    let (a, d) = (local_z0(-1.0, -1.0), local_z0(1.0, 1.0));
    let want = [a, [HI, a[1]], [HI, d[1]], d];
    let want_area = (((HI - a[0]) + (HI - d[0])) / 2.0 * (a[1] - d[1])).abs();
    for (backend, result) in on_every_backend(&[z0(&poly)]) {
        let out = result.unwrap_or_else(|e| panic!("{backend}: {e}"));
        let ClippedGeometry::Polygons(polys) = &out[0] else {
            panic!("{backend}: expected a polygon, got {:?}", out[0]);
        };
        let ring = &polys[0].exterior;
        assert!(all_finite(ring), "{backend}: {ring:?}");
        assert!(
            (shoelace(ring).abs() / want_area - 1.0).abs() < 1e-4,
            "{backend}: {ring:?}"
        );
        for w in want {
            assert!(
                ring.iter().any(|&p| near(p, w, 0.01)),
                "{backend}: {w:?} missing from {ring:?}"
            );
        }
    }
}

#[test]
fn a_line_to_a_huge_longitude_still_crosses_the_tile() {
    let line = Geometry::Line(LineString(vec![coord(-1.0, -1.0), coord(1e300, -1.0)]));
    let start = local_z0(-1.0, -1.0);
    for (backend, result) in on_every_backend(&[z0(&line)]) {
        let out = result.unwrap_or_else(|e| panic!("{backend}: {e}"));
        let ClippedGeometry::Lines(parts) = &out[0] else {
            panic!("{backend}: expected a line, got {:?}", out[0]);
        };
        assert_eq!(parts.len(), 1, "{backend}: {parts:?}");
        assert_eq!(parts[0].len(), 2, "{backend}: {parts:?}");
        assert!(near(parts[0][0], start, 0.01), "{backend}: {parts:?}");
        assert!(
            near(parts[0][1], [HI, start[1]], 0.01),
            "{backend}: {parts:?}"
        );
    }
}

#[test]
fn coordinates_too_far_to_project_are_invalid_input() {
    // At z30 with extent 2^24 even the f64 projection of lon 1e300
    // overflows.
    let line = Geometry::Line(LineString(vec![coord(-1.0, -1.0), coord(1e300, -1.0)]));
    let item = WorkItem {
        geometry: &line,
        tile_x: 0,
        tile_y: 0,
        zoom: 30,
        extent: 1 << 24,
    };
    for (backend, result) in on_every_backend(&[item]) {
        assert!(
            matches!(result, Err(AccelError::InvalidInput(_))),
            "{backend}: {result:?}"
        );
    }
}

/// A straight line in tile-local (projected) space, given by two points
/// relative to tile `(tx, ty)` at zoom `z`, as lon/lat.
fn local_to_lonlat(z: u8, tx: u32, ty: u32, p: [f64; 2]) -> Coord<f64> {
    let n = f64::from(1u32 << z);
    coord(
        unit_x_to_lon((f64::from(tx) + p[0] / 4096.0) / n),
        unit_y_to_lat((f64::from(ty) + p[1] / 4096.0) / n),
    )
}

/// `y` on the line through `p` and `q` at `x`, in f64.
fn y_at(p: [f64; 2], q: [f64; 2], x: f64) -> f64 {
    p[1] + (x - p[0]) * (q[1] - p[1]) / (q[0] - p[0])
}

#[test]
fn an_edge_spanning_thousands_of_tiles_keeps_its_precision() {
    // z14: a slanted line from 1500 tiles west to 1501 tiles east. In f32
    // its far end is only representable to ~0.5 units, which moved the
    // tile crossings by ~0.3 units before the guard-box pre-clip.
    let (z, tx, ty) = (14, 8000, 6000);
    let p = [-1500.0 * 4096.0 + 0.37, 1000.3];
    let q = [1501.0 * 4096.0 + 0.21, 3000.7];
    let line = Geometry::Line(LineString(vec![
        local_to_lonlat(z, tx, ty, p),
        local_to_lonlat(z, tx, ty, q),
    ]));
    let item = WorkItem {
        geometry: &line,
        tile_x: tx,
        tile_y: ty,
        zoom: z,
        extent: 4096,
    };
    let want = [[LO, y_at(p, q, LO)], [HI, y_at(p, q, HI)]];
    for (backend, result) in on_every_backend(&[item]) {
        let out = result.unwrap_or_else(|e| panic!("{backend}: {e}"));
        let ClippedGeometry::Lines(parts) = &out[0] else {
            panic!("{backend}: expected a line, got {:?}", out[0]);
        };
        assert_eq!(parts.len(), 1, "{backend}: {parts:?}");
        let part = &parts[0];
        assert!(
            near(part[0], want[0], 0.01) && near(part[part.len() - 1], want[1], 0.01),
            "{backend}: {part:?} vs {want:?}"
        );
    }
}

#[test]
fn a_ring_spanning_thousands_of_tiles_keeps_its_precision() {
    // z18: a triangle whose upper edge runs 20000 tiles each way and
    // crosses the tile diagonally; the rest of the tile lies inside the
    // triangle. Rounded to f32, the far vertices move by up to 2 units.
    let (z, tx, ty) = (18, 100_000, 50_000);
    let a = [-20_000.0 * 4096.0 + 0.4, -10_000.0 * 4096.0 + 0.3];
    let b = [20_000.0 * 4096.0 + 0.8, 10_000.0 * 4096.0 + 2049.7];
    let c = [0.0, 200_000.0 * 4096.0];
    let ring = LineString(
        [a, b, c, a]
            .into_iter()
            .map(|p| local_to_lonlat(z, tx, ty, p))
            .collect(),
    );
    let poly = Geometry::Polygon(Polygon::new(ring, vec![]));
    let item = WorkItem {
        geometry: &poly,
        tile_x: tx,
        tile_y: ty,
        zoom: z,
        extent: 4096,
    };
    let want = [
        [LO, y_at(a, b, LO)],
        [HI, y_at(a, b, HI)],
        [HI, HI],
        [LO, HI],
    ];
    for (backend, result) in on_every_backend(&[item]) {
        let out = result.unwrap_or_else(|e| panic!("{backend}: {e}"));
        let ClippedGeometry::Polygons(polys) = &out[0] else {
            panic!("{backend}: expected a polygon, got {:?}", out[0]);
        };
        let ring = &polys[0].exterior;
        assert!(all_finite(ring), "{backend}: {ring:?}");
        for w in want {
            assert!(
                ring.iter().any(|&p| near(p, w, 0.01)),
                "{backend}: {w:?} missing from {ring:?}"
            );
        }
        assert!(
            ring.iter().all(|&p| want.iter().any(|&w| near(p, w, 0.01))),
            "{backend}: unexpected vertex in {ring:?}"
        );
    }
}
