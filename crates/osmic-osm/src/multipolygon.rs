//! Area assembly for `type=multipolygon` and `type=boundary` relations.
//!
//! The algorithm follows libosmium's area assembler, working on OSM's exact
//! fixed-point coordinates so that shared nodes always match:
//!
//! 1. **Segments.** Every member way is split into segments. Zero-length
//!    segments are dropped. A segment that appears an even number of times
//!    (two rings sharing an edge, a way listed twice) cancels out; an odd
//!    count leaves one copy.
//! 2. **Validation.** Every vertex must have even degree. An odd-degree
//!    vertex means a ring cannot close — usually a member way missing from
//!    an extract — and the relation is rejected rather than emitted as a
//!    partial polygon.
//! 3. **Rings.** Segments are walked into closed circuits *ignoring member
//!    roles* (roles are frequently wrong in real data). Each circuit is split
//!    at repeated vertices, so rings that touch at a point become separate
//!    simple rings.
//! 4. **Nesting.** Each ring's parent is the smallest ring containing it,
//!    tested with an exact integer point-in-ring check on a vertex that is
//!    not on the parent's boundary. Even nesting depth means outer ring, odd
//!    means hole — so islands in lakes in land come out right regardless of
//!    roles. Roles that disagree with the geometry are counted, not trusted.
//! 5. **Output.** One polygon per outer ring with its direct holes; exteriors
//!    counter-clockwise, holes clockwise.

use geo_types::{Coord, LineString, MultiPolygon, Polygon};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use osmic_core::{FixedCoord, Geometry};

/// Member role of a way in an area relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Outer,
    Inner,
    /// Empty role — treated as outer by convention, never counted as a
    /// mismatch.
    Empty,
}

impl Role {
    /// Parse a relation member role; `None` for roles that do not take part
    /// in area assembly (`label`, `admin_centre`, `subarea`, …).
    pub fn parse(role: &str) -> Option<Self> {
        match role {
            "outer" => Some(Self::Outer),
            "inner" => Some(Self::Inner),
            "" => Some(Self::Empty),
            _ => None,
        }
    }
}

/// A member way with resolved coordinates.
#[derive(Debug, Clone, Copy)]
pub struct MemberWay<'a> {
    pub id: i64,
    pub role: Role,
    pub coords: &'a [FixedCoord],
}

/// Why a relation produced no area.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AssemblyError {
    #[error("relation has no member ways with geometry")]
    NoMembers,
    #[error("ring cannot be closed at {lon},{lat} (missing or broken member way)")]
    OpenRing { lon: i32, lat: i32 },
    #[error("no ring encloses a positive area")]
    NoArea,
}

/// Diagnostics from a successful assembly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssemblyReport {
    pub outer_rings: usize,
    pub inner_rings: usize,
    /// Segments whose member role disagreed with the ring they ended up in.
    pub role_mismatches: usize,
    /// Segments removed because they appeared twice.
    pub duplicate_segments: usize,
}

/// Assemble the area described by `members`.
pub fn assemble_area(
    members: &[MemberWay<'_>],
) -> Result<(Geometry, AssemblyReport), AssemblyError> {
    let mut report = AssemblyReport::default();

    // ── 1. Segments, cancelling duplicates ────────────────────────────────
    let mut counts: FxHashMap<(FixedCoord, FixedCoord), (u32, Role)> = FxHashMap::default();
    let mut order: Vec<(FixedCoord, FixedCoord)> = Vec::new();
    for m in members {
        for w in m.coords.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a == b {
                continue;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            let e = counts.entry(key).or_insert_with(|| {
                order.push(key);
                (0, m.role)
            });
            e.0 += 1;
        }
    }
    if order.is_empty() {
        return Err(AssemblyError::NoMembers);
    }
    let mut segments: Vec<Segment> = Vec::with_capacity(order.len());
    for key in order {
        let (count, role) = counts[&key];
        report.duplicate_segments += (count - count % 2) as usize;
        if count % 2 == 1 {
            segments.push(Segment {
                a: key.0,
                b: key.1,
                role,
                used: false,
            });
        }
    }
    if segments.is_empty() {
        return Err(AssemblyError::NoArea);
    }

    // ── 2. Endpoint index and degree check ────────────────────────────────
    let mut adjacency: FxHashMap<FixedCoord, SmallVec<[u32; 2]>> = FxHashMap::default();
    for (i, s) in segments.iter().enumerate() {
        adjacency.entry(s.a).or_default().push(i as u32);
        adjacency.entry(s.b).or_default().push(i as u32);
    }
    // Deterministic choice of the reported vertex: smallest odd one.
    if let Some(odd) = adjacency
        .iter()
        .filter(|(_, segs)| segs.len() % 2 == 1)
        .map(|(c, _)| *c)
        .min()
    {
        return Err(AssemblyError::OpenRing {
            lon: odd.lon,
            lat: odd.lat,
        });
    }

    // ── 3. Closed walks, split into simple rings ──────────────────────────
    let mut rings: Vec<Ring> = Vec::new();
    for start in 0..segments.len() {
        if segments[start].used {
            continue;
        }
        segments[start].used = true;
        let origin = segments[start].a;
        let mut walk: Vec<(FixedCoord, u32)> = vec![(origin, start as u32)];
        let mut current = segments[start].b;
        while current != origin {
            // Even degrees guarantee an unused exit from every vertex except
            // the origin until the walk returns there.
            let Some(next) = adjacency
                .get(&current)
                .and_then(|segs| segs.iter().copied().find(|&s| !segments[s as usize].used))
            else {
                return Err(AssemblyError::OpenRing {
                    lon: current.lon,
                    lat: current.lat,
                });
            };
            let seg = &mut segments[next as usize];
            seg.used = true;
            walk.push((current, next));
            current = if seg.a == current { seg.b } else { seg.a };
        }
        split_into_simple_rings(&walk, &segments, &mut rings);
    }
    rings.retain(|r| r.area2 != 0);
    if rings.is_empty() {
        return Err(AssemblyError::NoArea);
    }

    // ── 4. Nesting ─────────────────────────────────────────────────────────
    // Largest first; ties broken by first vertex for determinism.
    rings.sort_by(|x, y| {
        y.area2
            .abs()
            .cmp(&x.area2.abs())
            .then_with(|| x.coords[0].cmp(&y.coords[0]))
    });
    let mut parent: Vec<Option<usize>> = vec![None; rings.len()];
    let mut depth: Vec<usize> = vec![0; rings.len()];
    for i in 0..rings.len() {
        // Candidates are larger rings; scan from the smallest of them so the
        // first container found is the tightest one.
        for j in (0..i).rev() {
            if rings[j].contains_ring(&rings[i]) {
                parent[i] = Some(j);
                depth[i] = depth[j] + 1;
                break;
            }
        }
    }

    for (i, ring) in rings.iter().enumerate() {
        let is_outer = depth[i].is_multiple_of(2);
        if is_outer {
            report.outer_rings += 1;
        } else {
            report.inner_rings += 1;
        }
        report.role_mismatches += ring
            .roles
            .iter()
            .filter(|r| match r {
                Role::Outer => !is_outer,
                Role::Inner => is_outer,
                Role::Empty => false,
            })
            .count();
    }

    // ── 5. Polygons ────────────────────────────────────────────────────────
    let mut polygons: Vec<(usize, Vec<LineString<f64>>)> = Vec::new();
    let mut polygon_of: Vec<Option<usize>> = vec![None; rings.len()];
    for i in 0..rings.len() {
        if depth[i].is_multiple_of(2) {
            polygon_of[i] = Some(polygons.len());
            polygons.push((i, Vec::new()));
        } else if let Some(p) = parent[i].and_then(|p| polygon_of[p]) {
            polygons[p].1.push(rings[i].to_linestring(false));
        }
    }
    let mut polys: Vec<Polygon<f64>> = polygons
        .into_iter()
        .map(|(outer, holes)| Polygon::new(rings[outer].to_linestring(true), holes))
        .collect();

    let geometry = if polys.len() == 1 {
        Geometry::Polygon(polys.remove(0))
    } else {
        Geometry::MultiPolygon(MultiPolygon(polys))
    };
    Ok((geometry, report))
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    a: FixedCoord,
    b: FixedCoord,
    role: Role,
    used: bool,
}

#[derive(Debug)]
struct Ring {
    /// Closed (first == last).
    coords: Vec<FixedCoord>,
    /// Twice the signed area (positive = counter-clockwise).
    area2: i128,
    min: FixedCoord,
    max: FixedCoord,
    roles: Vec<Role>,
}

impl Ring {
    fn new(coords: Vec<FixedCoord>, roles: Vec<Role>) -> Self {
        let mut area2: i128 = 0;
        let mut min = coords[0];
        let mut max = coords[0];
        for w in coords.windows(2) {
            let (a, b) = (w[0], w[1]);
            area2 += i128::from(a.lon) * i128::from(b.lat) - i128::from(b.lon) * i128::from(a.lat);
            min.lon = min.lon.min(b.lon);
            min.lat = min.lat.min(b.lat);
            max.lon = max.lon.max(b.lon);
            max.lat = max.lat.max(b.lat);
        }
        Self {
            coords,
            area2,
            min,
            max,
            roles,
        }
    }

    fn bbox_contains(&self, other: &Ring) -> bool {
        self.min.lon <= other.min.lon
            && self.min.lat <= other.min.lat
            && self.max.lon >= other.max.lon
            && self.max.lat >= other.max.lat
    }

    /// Whether `other` lies inside this ring. Rings never cross, so the
    /// first vertex of `other` that is not on this ring's boundary decides.
    fn contains_ring(&self, other: &Ring) -> bool {
        if !self.bbox_contains(other) {
            return false;
        }
        for &p in &other.coords {
            match point_in_ring(p, &self.coords) {
                Location::Inside => return true,
                Location::Outside => return false,
                Location::Boundary => {}
            }
        }
        false
    }

    fn to_linestring(&self, ccw: bool) -> LineString<f64> {
        let mut coords: Vec<Coord<f64>> = self.coords.iter().map(|c| c.to_coord()).collect();
        if (self.area2 > 0) != ccw {
            coords.reverse();
        }
        LineString(coords)
    }
}

enum Location {
    Inside,
    Outside,
    Boundary,
}

/// Exact point-in-ring test (even–odd crossing count) on fixed-point
/// coordinates; `ring` is closed.
fn point_in_ring(p: FixedCoord, ring: &[FixedCoord]) -> Location {
    let (px, py) = (i128::from(p.lon), i128::from(p.lat));
    let mut inside = false;
    for w in ring.windows(2) {
        let (ax, ay) = (i128::from(w[0].lon), i128::from(w[0].lat));
        let (bx, by) = (i128::from(w[1].lon), i128::from(w[1].lat));
        let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
        if cross == 0
            && px >= ax.min(bx)
            && px <= ax.max(bx)
            && py >= ay.min(by)
            && py <= ay.max(by)
        {
            return Location::Boundary;
        }
        if (ay > py) != (by > py) {
            // The edge crosses the horizontal line through p; count it if
            // the crossing lies to the right of p. With by > ay the crossing
            // is right of p exactly when cross > 0.
            let right = if by > ay { cross > 0 } else { cross < 0 };
            if right {
                inside = !inside;
            }
        }
    }
    if inside {
        Location::Inside
    } else {
        Location::Outside
    }
}

/// Split a closed walk into simple rings at repeated vertices. `walk[i]` is
/// (vertex, segment leaving it); the walk ends back at `walk[0].0`.
fn split_into_simple_rings(walk: &[(FixedCoord, u32)], segments: &[Segment], out: &mut Vec<Ring>) {
    let mut stack: Vec<(FixedCoord, Role)> = Vec::with_capacity(walk.len() + 1);
    let mut position: FxHashMap<FixedCoord, usize> = FxHashMap::default();
    let vertices = walk
        .iter()
        .map(|&(v, s)| (v, segments[s as usize].role))
        .chain(std::iter::once((walk[0].0, Role::Empty)));
    for (v, role) in vertices {
        if let Some(&start) = position.get(&v) {
            // Close the loop stack[start..] + v.
            let loop_part = stack.split_off(start);
            for (c, _) in &loop_part {
                position.remove(c);
            }
            let roles: Vec<Role> = loop_part.iter().map(|&(_, r)| r).collect();
            let mut coords: Vec<FixedCoord> = loop_part.into_iter().map(|(c, _)| c).collect();
            coords.push(v);
            if coords.len() >= 4 {
                out.push(Ring::new(coords, roles));
            }
        }
        position.insert(v, stack.len());
        stack.push((v, role));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Area, Winding};

    fn f(x: i32, y: i32) -> FixedCoord {
        FixedCoord::new(x, y)
    }

    fn square(x0: i32, y0: i32, size: i32) -> Vec<FixedCoord> {
        vec![
            f(x0, y0),
            f(x0 + size, y0),
            f(x0 + size, y0 + size),
            f(x0, y0 + size),
            f(x0, y0),
        ]
    }

    fn way<'a>(id: i64, role: Role, coords: &'a [FixedCoord]) -> MemberWay<'a> {
        MemberWay { id, role, coords }
    }

    fn polygons(g: &Geometry) -> Vec<&Polygon<f64>> {
        match g {
            Geometry::Polygon(p) => vec![p],
            Geometry::MultiPolygon(mp) => mp.0.iter().collect(),
            other => panic!("not an area: {other:?}"),
        }
    }

    #[test]
    fn single_closed_way() {
        let s = square(0, 0, 10);
        let (g, r) = assemble_area(&[way(1, Role::Outer, &s)]).expect("valid");
        let p = polygons(&g);
        assert_eq!(p.len(), 1);
        assert!(p[0].exterior().is_ccw());
        assert_eq!(r.outer_rings, 1);
        assert_eq!(r.role_mismatches, 0);
    }

    #[test]
    fn ring_from_several_ways_in_any_order_and_direction() {
        let a = [f(0, 0), f(10, 0)];
        let b = [f(0, 10), f(10, 10)]; // reversed relative to the ring
        let c = [f(10, 0), f(10, 10)];
        let d = [f(0, 10), f(0, 0)];
        let (g, _) = assemble_area(&[
            way(1, Role::Outer, &c),
            way(2, Role::Outer, &a),
            way(3, Role::Outer, &d),
            way(4, Role::Outer, &b),
        ])
        .expect("closes");
        assert_eq!(polygons(&g)[0].exterior().0.len(), 5);
    }

    #[test]
    fn hole_and_island_in_hole_nest_by_geometry() {
        let land = square(0, 0, 100);
        let lake = square(10, 10, 80);
        let island = square(40, 40, 20);
        let (g, r) = assemble_area(&[
            way(1, Role::Outer, &land),
            way(2, Role::Inner, &lake),
            way(3, Role::Outer, &island),
        ])
        .expect("valid");
        let p = polygons(&g);
        assert_eq!(p.len(), 2, "land-with-lake and island");
        assert_eq!(p[0].interiors().len(), 1);
        assert!(p[0].interiors()[0].is_cw());
        assert!(p[1].interiors().is_empty());
        assert_eq!((r.outer_rings, r.inner_rings, r.role_mismatches), (2, 1, 0));
    }

    #[test]
    fn wrong_roles_are_counted_not_trusted() {
        let outer = square(0, 0, 100);
        let hole = square(10, 10, 10);
        let (g, r) = assemble_area(&[way(1, Role::Inner, &outer), way(2, Role::Outer, &hole)])
            .expect("valid");
        let p = polygons(&g);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].interiors().len(), 1);
        assert_eq!(r.role_mismatches, 8, "every segment of both rings");
    }

    #[test]
    fn inner_touching_outer_at_a_vertex_is_kept() {
        let outer = square(0, 0, 100);
        // Triangle hole touching the outer ring at (0, 0).
        let hole = vec![f(0, 0), f(20, 10), f(10, 20), f(0, 0)];
        let (g, r) = assemble_area(&[way(1, Role::Outer, &outer), way(2, Role::Inner, &hole)])
            .expect("valid");
        let p = polygons(&g);
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0].interiors().len(),
            1,
            "touching hole must not be dropped"
        );
        assert_eq!(r.inner_rings, 1);
        let area = p[0].unsigned_area();
        assert!(
            (area - (100.0 * 100.0 - 150.0) * 1e-14).abs() < 1e-12,
            "{area}"
        );
    }

    #[test]
    fn figure_eight_splits_into_two_polygons() {
        // Two squares sharing one corner, drawn as one closed way.
        let eight = vec![
            f(0, 0),
            f(10, 0),
            f(10, 10),
            f(20, 10),
            f(20, 20),
            f(10, 20),
            f(10, 10),
            f(0, 10),
            f(0, 0),
        ];
        let (g, r) = assemble_area(&[way(1, Role::Outer, &eight)]).expect("valid");
        assert_eq!(polygons(&g).len(), 2);
        assert_eq!(r.outer_rings, 2);
    }

    #[test]
    fn shared_edge_between_touching_rings_cancels() {
        // Two adjacent squares tagged as one area: the shared edge cancels
        // and the result is one rectangle.
        let left = square(0, 0, 10);
        let right = square(10, 0, 10);
        let (g, r) = assemble_area(&[way(1, Role::Outer, &left), way(2, Role::Outer, &right)])
            .expect("valid");
        let p = polygons(&g);
        assert_eq!(p.len(), 1);
        assert_eq!(r.duplicate_segments, 2);
        assert!((p[0].unsigned_area() - 200.0 * 1e-14).abs() < 1e-15);
    }

    #[test]
    fn missing_member_is_an_error_not_a_partial_polygon() {
        let a = [f(0, 0), f(10, 0), f(10, 10)];
        let b = [f(10, 10), f(0, 10)]; // ring never returns to (0, 0)
        let err = assemble_area(&[way(1, Role::Outer, &a), way(2, Role::Outer, &b)]).unwrap_err();
        assert!(matches!(err, AssemblyError::OpenRing { .. }));
    }

    #[test]
    fn empty_and_degenerate_input() {
        assert_eq!(assemble_area(&[]).unwrap_err(), AssemblyError::NoMembers);
        let dup = [f(0, 0), f(0, 0)];
        assert_eq!(
            assemble_area(&[way(1, Role::Outer, &dup)]).unwrap_err(),
            AssemblyError::NoMembers
        );
        // A way and its exact reverse cancel completely.
        let line = [f(0, 0), f(10, 0), f(10, 10)];
        let back = [f(10, 10), f(10, 0), f(0, 0)];
        assert_eq!(
            assemble_area(&[way(1, Role::Outer, &line), way(2, Role::Outer, &back)]).unwrap_err(),
            AssemblyError::NoArea
        );
    }

    #[test]
    fn many_rings_scale() {
        // 400 small outer squares with holes: exercises nesting with many rings.
        let mut storage = Vec::new();
        for i in 0..20 {
            for j in 0..20 {
                storage.push((square(i * 100, j * 100, 50), Role::Outer));
                storage.push((square(i * 100 + 10, j * 100 + 10, 10), Role::Inner));
            }
        }
        let members: Vec<_> = storage
            .iter()
            .enumerate()
            .map(|(k, (c, r))| way(k as i64, *r, c))
            .collect();
        let (g, r) = assemble_area(&members).expect("valid");
        assert_eq!(polygons(&g).len(), 400);
        assert_eq!(
            (r.outer_rings, r.inner_rings, r.role_mismatches),
            (400, 400, 0)
        );
    }
}
