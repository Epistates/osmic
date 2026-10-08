//! Planar clipping of points, lines and polygons to axis-aligned rectangles.
//!
//! Everything here works in any planar coordinate space — geographic degrees
//! or projected tile pixels — so the same code serves geographic clipping and
//! the tile renderer. A [`BBox`] is used as the rectangle type; in a
//! projected space read `min_lon`/`max_lon` as x and `min_lat`/`max_lat` as y.
//!
//! The primitives clip to a *band* along one axis (`min <= coord <= max`); a
//! rectangle is a band in x followed by a band in y. Bands are also what the
//! tile renderer needs to split geometry recursively into tile columns and
//! rows.
//!
//! Semantics:
//! - Lines that leave and re-enter the rectangle become several parts; no
//!   part is ever dropped.
//! - Polygon rings are clipped with Sutherland–Hodgman. Concave rings that
//!   cross the rectangle several times stay one ring joined by zero-width
//!   edges along the rectangle boundary — the standard trade-off made by
//!   geojson-vt and Planetiler; when clipping to a buffered tile those edges
//!   fall in the invisible buffer. Ring orientation is preserved.
//! - Intersection points are computed from the edge in a canonical
//!   direction, so the two tiles sharing a boundary produce bit-identical
//!   points for the same edge.

use geo_types::{Coord, LineString, MultiLineString, MultiPoint, MultiPolygon, Polygon};

use crate::bbox::BBox;
use crate::geometry::Geometry;

/// Coordinate axis for band clipping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Horizontal: longitude, or projected x.
    X,
    /// Vertical: latitude, or projected y.
    Y,
}

impl Axis {
    #[inline]
    fn get(self, c: Coord<f64>) -> f64 {
        match self {
            Self::X => c.x,
            Self::Y => c.y,
        }
    }
}

/// Point where segment `a`–`b` crosses `axis == k`. The axis coordinate is
/// set exactly to `k`, and the computation is ordered by axis value so the
/// result does not depend on the segment's direction.
#[inline]
fn intersect(a: Coord<f64>, b: Coord<f64>, axis: Axis, k: f64) -> Coord<f64> {
    let (lo, hi) = if axis.get(a) <= axis.get(b) {
        (a, b)
    } else {
        (b, a)
    };
    let t = (k - axis.get(lo)) / (axis.get(hi) - axis.get(lo));
    match axis {
        Axis::X => Coord {
            x: k,
            y: lo.y + t * (hi.y - lo.y),
        },
        Axis::Y => Coord {
            x: lo.x + t * (hi.x - lo.x),
            y: k,
        },
    }
}

/// Clip a polyline to `min <= axis <= max`, appending every resulting part
/// (each with at least two points) to `out`.
pub fn clip_line_band(
    pts: &[Coord<f64>],
    axis: Axis,
    min: f64,
    max: f64,
    out: &mut Vec<Vec<Coord<f64>>>,
) {
    fn flush(part: &mut Vec<Coord<f64>>, out: &mut Vec<Vec<Coord<f64>>>) {
        if part.len() >= 2 {
            out.push(std::mem::take(part));
        } else {
            part.clear();
        }
    }

    let mut part: Vec<Coord<f64>> = Vec::new();
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (ak, bk) = (axis.get(a), axis.get(b));
        let a_in = ak >= min && ak <= max;
        let b_in = bk >= min && bk <= max;

        // Where the segment enters the band. A segment that starts below
        // and ends above (or vice versa) enters at the near edge and is
        // given its exit point below.
        let start = if a_in {
            a
        } else if ak < min && bk >= min {
            intersect(a, b, axis, min)
        } else if ak > max && bk <= max {
            intersect(a, b, axis, max)
        } else {
            // Both endpoints on the same side: nothing inside.
            flush(&mut part, out);
            continue;
        };

        // Where it leaves (if it leaves through either edge).
        let end = if b_in {
            b
        } else if bk > max {
            intersect(a, b, axis, max)
        } else {
            intersect(a, b, axis, min)
        };

        if part.last() != Some(&start) {
            flush(&mut part, out);
            part.push(start);
        }
        if end != start {
            part.push(end);
        }
        if !b_in {
            flush(&mut part, out);
        }
    }
    flush(&mut part, out);
}

/// Sutherland–Hodgman against one half-plane.
fn clip_ring_half_plane(
    input: &[Coord<f64>],
    axis: Axis,
    k: f64,
    keep_greater: bool,
) -> Vec<Coord<f64>> {
    let mut out = Vec::with_capacity(input.len() + 4);
    let Some(&last) = input.last() else {
        return out;
    };
    let inside = |c: Coord<f64>| {
        if keep_greater {
            axis.get(c) >= k
        } else {
            axis.get(c) <= k
        }
    };
    let mut prev = last;
    let mut prev_in = inside(prev);
    for &cur in input {
        let cur_in = inside(cur);
        if cur_in {
            if !prev_in {
                out.push(intersect(prev, cur, axis, k));
            }
            out.push(cur);
        } else if prev_in {
            out.push(intersect(prev, cur, axis, k));
        }
        prev = cur;
        prev_in = cur_in;
    }
    out
}

/// Strip the closing vertex and consecutive duplicates; returns an open ring.
fn open_ring(ring: &[Coord<f64>]) -> Vec<Coord<f64>> {
    let mut v: Vec<Coord<f64>> = Vec::with_capacity(ring.len());
    for &c in ring {
        if v.last() != Some(&c) {
            v.push(c);
        }
    }
    while v.len() > 1 && v.first() == v.last() {
        v.pop();
    }
    v
}

/// Twice the signed area of an open ring (positive = counter-clockwise in a
/// y-up space).
pub fn ring_signed_area_2x(ring: &[Coord<f64>]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        sum += a.x * b.y - b.x * a.y;
    }
    sum
}

/// Close an open ring if it still describes an area; otherwise empty.
fn close_ring(mut v: Vec<Coord<f64>>) -> Vec<Coord<f64>> {
    v.dedup();
    while v.len() > 1 && v.first() == v.last() {
        v.pop();
    }
    if v.len() < 3 || ring_signed_area_2x(&v) == 0.0 {
        return Vec::new();
    }
    let first = v[0];
    v.push(first);
    v
}

/// Clip a ring (open or closed) to `min <= axis <= max`. Returns a closed
/// ring, or an empty vector if nothing with positive area remains.
pub fn clip_ring_band(ring: &[Coord<f64>], axis: Axis, min: f64, max: f64) -> Vec<Coord<f64>> {
    let open = open_ring(ring);
    let lower = clip_ring_half_plane(&open, axis, min, true);
    let both = clip_ring_half_plane(&lower, axis, max, false);
    close_ring(both)
}

/// Clip a ring to a rectangle. Returns a closed ring or an empty vector.
pub fn clip_ring(ring: &[Coord<f64>], rect: &BBox) -> Vec<Coord<f64>> {
    let open = open_ring(ring);
    let x1 = clip_ring_half_plane(&open, Axis::X, rect.min_lon, true);
    let x2 = clip_ring_half_plane(&x1, Axis::X, rect.max_lon, false);
    let y1 = clip_ring_half_plane(&x2, Axis::Y, rect.min_lat, true);
    let y2 = clip_ring_half_plane(&y1, Axis::Y, rect.max_lat, false);
    close_ring(y2)
}

/// Clip a polyline to a rectangle, returning every part that remains.
pub fn clip_line(line: &LineString<f64>, rect: &BBox) -> Vec<LineString<f64>> {
    let mut x_parts = Vec::new();
    clip_line_band(&line.0, Axis::X, rect.min_lon, rect.max_lon, &mut x_parts);
    let mut parts = Vec::new();
    for p in &x_parts {
        clip_line_band(p, Axis::Y, rect.min_lat, rect.max_lat, &mut parts);
    }
    parts.into_iter().map(LineString::new).collect()
}

/// Clip a polygon to a rectangle. Holes that vanish are dropped; returns
/// `None` if the exterior vanishes.
pub fn clip_polygon(poly: &Polygon<f64>, rect: &BBox) -> Option<Polygon<f64>> {
    let exterior = clip_ring(&poly.exterior().0, rect);
    if exterior.is_empty() {
        return None;
    }
    let interiors = poly
        .interiors()
        .iter()
        .map(|r| clip_ring(&r.0, rect))
        .filter(|r| !r.is_empty())
        .map(LineString::new)
        .collect();
    Some(Polygon::new(LineString::new(exterior), interiors))
}

/// Grow a rectangle by `fraction` of its width/height on every side.
pub fn buffered(rect: &BBox, fraction: f64) -> BBox {
    if fraction <= 0.0 {
        return *rect;
    }
    let bw = rect.width() * fraction;
    let bh = rect.height() * fraction;
    BBox::new(
        rect.min_lon - bw,
        rect.min_lat - bh,
        rect.max_lon + bw,
        rect.max_lat + bh,
    )
}

/// Clip any geometry to `rect` grown by `buffer_fraction`.
///
/// Returns `None` if nothing remains. Lines that cross the rectangle several
/// times come back as [`Geometry::MultiLine`].
pub fn clip_geometry(geom: &Geometry, rect: &BBox, buffer_fraction: f64) -> Option<Geometry> {
    let rect = buffered(rect, buffer_fraction);
    let lines = |ls: &mut dyn Iterator<Item = &LineString<f64>>| {
        let parts: Vec<LineString<f64>> = ls.flat_map(|l| clip_line(l, &rect)).collect();
        match parts.len() {
            0 => None,
            1 => parts.into_iter().next().map(Geometry::Line),
            _ => Some(Geometry::MultiLine(MultiLineString(parts))),
        }
    };
    let polygons = |ps: &mut dyn Iterator<Item = &Polygon<f64>>| {
        let polys: Vec<Polygon<f64>> = ps.filter_map(|p| clip_polygon(p, &rect)).collect();
        match polys.len() {
            0 => None,
            1 => polys.into_iter().next().map(Geometry::Polygon),
            _ => Some(Geometry::MultiPolygon(MultiPolygon(polys))),
        }
    };
    match geom {
        Geometry::Point(p) => rect.contains_point(p.x(), p.y()).then_some(geom.clone()),
        Geometry::MultiPoint(mp) => {
            let pts: Vec<_> =
                mp.0.iter()
                    .filter(|p| rect.contains_point(p.x(), p.y()))
                    .copied()
                    .collect();
            match pts.len() {
                0 => None,
                1 => Some(Geometry::Point(pts[0])),
                _ => Some(Geometry::MultiPoint(MultiPoint(pts))),
            }
        }
        Geometry::Line(ls) => lines(&mut std::iter::once(ls)),
        Geometry::MultiLine(mls) => lines(&mut mls.0.iter()),
        Geometry::Polygon(p) => polygons(&mut std::iter::once(p)),
        Geometry::MultiPolygon(mp) => polygons(&mut mp.0.iter()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: f64, y: f64) -> Coord<f64> {
        Coord { x, y }
    }

    fn unit() -> BBox {
        BBox::new(0.0, 0.0, 10.0, 10.0)
    }

    #[test]
    fn line_inside_is_unchanged() {
        let line = LineString::new(vec![c(1.0, 1.0), c(5.0, 5.0), c(9.0, 1.0)]);
        assert_eq!(clip_line(&line, &unit()), vec![line]);
    }

    #[test]
    fn line_outside_vanishes() {
        let line = LineString::new(vec![c(20.0, 20.0), c(30.0, 30.0)]);
        assert!(clip_line(&line, &unit()).is_empty());
    }

    #[test]
    fn line_leaving_and_reentering_keeps_both_parts() {
        // Enters, exits through the top, re-enters, ends inside.
        let line = LineString::new(vec![c(1.0, 5.0), c(3.0, 15.0), c(6.0, 15.0), c(8.0, 5.0)]);
        let parts = clip_line(&line, &unit());
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].0.first(), Some(&c(1.0, 5.0)));
        assert_eq!(parts[0].0.last().map(|p| p.y), Some(10.0));
        assert_eq!(parts[1].0.first().map(|p| p.y), Some(10.0));
        assert_eq!(parts[1].0.last(), Some(&c(8.0, 5.0)));
    }

    #[test]
    fn segment_crossing_whole_band_is_kept() {
        let line = LineString::new(vec![c(-5.0, 5.0), c(15.0, 5.0)]);
        let parts = clip_line(&line, &unit());
        assert_eq!(
            parts,
            vec![LineString::new(vec![c(0.0, 5.0), c(10.0, 5.0)])]
        );
    }

    #[test]
    fn clip_geometry_returns_multiline_for_multiple_parts() {
        let g = Geometry::Line(LineString::new(vec![
            c(1.0, 5.0),
            c(1.0, 15.0),
            c(9.0, 15.0),
            c(9.0, 5.0),
        ]));
        match clip_geometry(&g, &unit(), 0.0) {
            Some(Geometry::MultiLine(m)) => assert_eq!(m.0.len(), 2),
            other => panic!("expected MultiLine, got {other:?}"),
        }
    }

    #[test]
    fn polygon_partially_outside_is_cut_to_rect() {
        let poly = Polygon::new(
            LineString::new(vec![
                c(-5.0, -5.0),
                c(5.0, -5.0),
                c(5.0, 5.0),
                c(-5.0, 5.0),
                c(-5.0, -5.0),
            ]),
            vec![],
        );
        let out = clip_polygon(&poly, &unit()).expect("overlaps");
        let ring = &out.exterior().0;
        assert_eq!(ring.first(), ring.last());
        assert!(
            ring.iter()
                .all(|p| (0.0..=10.0).contains(&p.x) && (0.0..=10.0).contains(&p.y))
        );
        let area = ring_signed_area_2x(&ring[..ring.len() - 1]).abs() / 2.0;
        assert!((area - 25.0).abs() < 1e-9, "area {area}");
    }

    #[test]
    fn polygon_orientation_is_preserved() {
        let ccw = vec![
            c(-5.0, -5.0),
            c(5.0, -5.0),
            c(5.0, 5.0),
            c(-5.0, 5.0),
            c(-5.0, -5.0),
        ];
        let cw: Vec<_> = ccw.iter().rev().copied().collect();
        let a = clip_ring(&ccw, &unit());
        let b = clip_ring(&cw, &unit());
        assert!(ring_signed_area_2x(&a[..a.len() - 1]) > 0.0);
        assert!(ring_signed_area_2x(&b[..b.len() - 1]) < 0.0);
    }

    #[test]
    fn hole_outside_rect_is_dropped() {
        let poly = Polygon::new(
            LineString::new(vec![
                c(-20.0, -20.0),
                c(30.0, -20.0),
                c(30.0, 30.0),
                c(-20.0, 30.0),
                c(-20.0, -20.0),
            ]),
            vec![LineString::new(vec![
                c(20.0, 20.0),
                c(25.0, 20.0),
                c(25.0, 25.0),
                c(20.0, 25.0),
                c(20.0, 20.0),
            ])],
        );
        let out = clip_polygon(&poly, &unit()).expect("covers rect");
        assert!(out.interiors().is_empty());
    }

    #[test]
    fn polygon_outside_vanishes() {
        let poly = Polygon::new(
            LineString::new(vec![
                c(20.0, 20.0),
                c(30.0, 20.0),
                c(30.0, 30.0),
                c(20.0, 20.0),
            ]),
            vec![],
        );
        assert!(clip_polygon(&poly, &unit()).is_none());
    }

    #[test]
    fn shared_edge_clips_identically_from_both_sides() {
        // The same edge, traversed in opposite directions, must produce the
        // same boundary point so neighbouring tiles line up exactly.
        let a = c(-3.3, 1.7);
        let b = c(7.9, 9.1);
        assert_eq!(intersect(a, b, Axis::X, 0.0), intersect(b, a, Axis::X, 0.0));
    }

    #[test]
    fn concave_ring_stays_one_ring() {
        // A "U" crossing the bottom edge twice.
        let u = vec![
            c(1.0, -5.0),
            c(9.0, -5.0),
            c(9.0, 5.0),
            c(7.0, 5.0),
            c(7.0, -2.0),
            c(3.0, -2.0),
            c(3.0, 5.0),
            c(1.0, 5.0),
            c(1.0, -5.0),
        ];
        let out = clip_ring(&u, &unit());
        assert!(!out.is_empty());
        assert!(out.iter().all(|p| p.y >= 0.0));
    }
}
