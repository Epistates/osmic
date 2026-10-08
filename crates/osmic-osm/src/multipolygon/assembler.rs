//! Multipolygon assembly driver — segment-oriented ring closing with
//! orientation correction and inner-to-outer matching.
//!
//! The pipeline is:
//!
//! 1. **Extract segments.** `SegmentList::from_ways()` decomposes all member
//!    ways into directed segments and builds the endpoint index.
//! 2. **Close rings.** For each unused segment, start a `ProtoRing` and
//!    greedily chain matching segments by endpoint lookup. Continue until
//!    the ring is closed (last == first) or no more matching segments
//!    exist (then we emit an `OpenRing` warning and skip).
//! 3. **Correct orientation.** Each closed ring knows its signed area from
//!    the running shoelace sum — we flip outers to CCW and inners to CW.
//! 4. **Match inners to outers.** Each inner ring is tested against every
//!    outer ring via point-in-polygon (using the inner's first coord). The
//!    smallest enclosing outer wins; unmatched inners are reported as
//!    `OrphanedInner` warnings and discarded.
//! 5. **Build the final geometry.** Single outer → `Polygon`; multiple
//!    outers → `MultiPolygon`.
//!
//! Out-of-order ways, reversed segments, multi-outer relations, and
//! interleaved inner/outer iteration are all handled by the endpoint-index
//! lookup — it's the central primitive that makes this algorithm O(n log n)
//! in the number of segments rather than O(n²).

use std::collections::HashMap;

use geo::algorithm::area::Area;
use geo::algorithm::contains::Contains;
use geo_types::{Coord, LineString, MultiPolygon, Polygon};

use osmic_core::geometry::Geometry;

use crate::multipolygon::ring::ProtoRing;
use crate::multipolygon::segment::{Role, SegmentList, quantize};

/// Non-fatal warnings emitted during assembly. A relation with warnings
/// still produces a (possibly incomplete) geometry; the caller decides
/// whether to log them.
#[derive(Debug, Clone)]
pub enum AssemblyWarning {
    /// A ring could not be closed — the chainer ran out of matching segments
    /// before returning to the starting point.
    OpenRing { role: Role, start: Coord<f64> },
    /// An inner ring has no enclosing outer ring.
    OrphanedInner { first: Coord<f64> },
    /// A ring was built but has fewer than 4 coords (not a valid polygon).
    DegenerateRing { role: Role },
}

/// Fatal assembly errors — the relation produces no geometry at all.
#[derive(Debug, Clone)]
pub enum AssemblyError {
    /// No valid outer rings were produced. Cannot build a polygon.
    NoOuterRings,
}

/// Summary of what happened during assembly.
#[derive(Debug, Default, Clone)]
pub struct AssemblyReport {
    pub outer_rings_built: usize,
    pub inner_rings_built: usize,
    pub inner_rings_matched: usize,
    pub warnings: Vec<AssemblyWarning>,
}

/// Assemble a multipolygon from OSM relation members.
///
/// This replaces the legacy `chain_ways` + manual `Polygon` construction
/// with a robust segment-oriented approach that handles out-of-order ways,
/// multi-outer relations, and orientation correction.
pub fn assemble_multipolygon(
    outer_way_ids: &[i64],
    inner_way_ids: &[i64],
    way_geoms: &HashMap<i64, Vec<Coord<f64>>>,
) -> Result<(Geometry, AssemblyReport), AssemblyError> {
    let mut segments = SegmentList::from_ways(outer_way_ids, inner_way_ids, way_geoms);
    let mut report = AssemblyReport::default();

    // ── Phase 1: close rings ──────────────────────────────────────────
    let (mut outer_rings, mut inner_rings) = close_all_rings(&mut segments, &mut report);

    // ── Phase 2: orientation correction (OGC winding) ─────────────────
    for ring in &mut outer_rings {
        if ring.is_cw() {
            ring.reverse();
        }
    }
    for ring in &mut inner_rings {
        if ring.is_ccw() {
            ring.reverse();
        }
    }

    report.outer_rings_built = outer_rings.len();
    report.inner_rings_built = inner_rings.len();

    if outer_rings.is_empty() {
        return Err(AssemblyError::NoOuterRings);
    }

    // ── Phase 3: build outer polygons (without holes yet) ─────────────
    let mut outer_polys: Vec<Polygon<f64>> = outer_rings
        .into_iter()
        .map(|r| Polygon::new(LineString::new(r.coords), Vec::new()))
        .collect();

    // ── Phase 4: assign each inner to its smallest enclosing outer ───
    // We test inner.first() ∈ outer (Point in Polygon). The smallest
    // enclosing outer by area wins — this handles nested multipolygons
    // like "country with an island with a lake".
    let mut inner_by_outer: Vec<Vec<LineString<f64>>> = vec![Vec::new(); outer_polys.len()];
    for inner in inner_rings {
        let Some(probe) = inner.first() else {
            report
                .warnings
                .push(AssemblyWarning::DegenerateRing { role: Role::Inner });
            continue;
        };

        let mut best: Option<(usize, f64)> = None;
        for (i, outer) in outer_polys.iter().enumerate() {
            if outer.contains(&probe) {
                let area = outer.unsigned_area();
                match best {
                    Some((_, best_area)) if area >= best_area => {}
                    _ => best = Some((i, area)),
                }
            }
        }

        if let Some((idx, _)) = best {
            inner_by_outer[idx].push(LineString::new(inner.coords));
            report.inner_rings_matched += 1;
        } else {
            report
                .warnings
                .push(AssemblyWarning::OrphanedInner { first: probe });
        }
    }

    // ── Phase 5: attach holes back to their outer polygons ────────────
    for (poly, holes) in outer_polys.iter_mut().zip(inner_by_outer) {
        if holes.is_empty() {
            continue;
        }
        // Polygon has interiors_mut(FnOnce(&mut [LineString])) but no
        // replace-all helper, so we use the raw-parts round-trip via
        // into_inner() / Polygon::new.
        let (exterior, existing) =
            std::mem::replace(poly, Polygon::new(LineString::new(Vec::new()), Vec::new()))
                .into_inner();
        let mut all_holes = existing;
        all_holes.extend(holes);
        *poly = Polygon::new(exterior, all_holes);
    }

    // ── Phase 6: collapse into Polygon or MultiPolygon ────────────────
    let geometry = if outer_polys.len() == 1 {
        Geometry::Polygon(outer_polys.into_iter().next().unwrap())
    } else {
        Geometry::MultiPolygon(MultiPolygon::new(outer_polys))
    };

    Ok((geometry, report))
}

/// Walk the segment list, greedily closing rings. Emits `OpenRing` warnings
/// for any ring that can't be completed.
fn close_all_rings(
    segments: &mut SegmentList,
    report: &mut AssemblyReport,
) -> (Vec<ProtoRing>, Vec<ProtoRing>) {
    let mut outer_rings: Vec<ProtoRing> = Vec::new();
    let mut inner_rings: Vec<ProtoRing> = Vec::new();

    while let Some(seed_idx) = segments.find_first_unused() {
        let seed = *segments
            .get(seed_idx)
            .expect("find_first_unused returned out-of-range index");
        segments.mark_used(seed_idx);

        let mut ring = ProtoRing::new(seed.role);
        ring.append(seed.start, seed.end);

        loop {
            if ring.is_closed() {
                break;
            }

            // Find a continuation from the ring's current end point, same role.
            let Some(current_end) = ring.last() else {
                break;
            };
            let Some((next_idx, needs_reverse)) =
                segments.find_matching_unused(current_end, seed.role)
            else {
                // Dead end — emit warning and abandon this ring.
                report.warnings.push(AssemblyWarning::OpenRing {
                    role: seed.role,
                    start: ring.first().unwrap_or(seed.start),
                });
                break;
            };

            segments.mark_used(next_idx);
            let next = *segments.get(next_idx).unwrap();
            let (s, e) = if needs_reverse {
                (next.end, next.start)
            } else {
                (next.start, next.end)
            };

            // Defensive: verify continuity. Segments returned from
            // find_matching_unused must already align but quantization
            // rounding guarantees only key equality, not byte equality.
            debug_assert_eq!(
                quantize(s),
                quantize(current_end),
                "chainer produced discontinuous segment"
            );
            // Push using the ring's current end as the segment start to
            // preserve exact equality for the final ring closure test.
            ring.append(current_end, e);

            // Safety: bound the loop to prevent runaway on pathological data.
            if ring.coords.len() > segments.len() * 2 + 8 {
                report.warnings.push(AssemblyWarning::OpenRing {
                    role: seed.role,
                    start: ring.first().unwrap_or(seed.start),
                });
                break;
            }
        }

        if ring.is_closed() {
            match seed.role {
                Role::Outer => outer_rings.push(ring),
                Role::Inner => inner_rings.push(ring),
            }
        } else if !ring.is_empty() {
            report
                .warnings
                .push(AssemblyWarning::DegenerateRing { role: seed.role });
        }
    }

    (outer_rings, inner_rings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::algorithm::winding_order::Winding;

    fn c(x: f64, y: f64) -> Coord<f64> {
        Coord { x, y }
    }

    fn square_ccw() -> Vec<Coord<f64>> {
        vec![
            c(0.0, 0.0),
            c(1.0, 0.0),
            c(1.0, 1.0),
            c(0.0, 1.0),
            c(0.0, 0.0),
        ]
    }

    fn square_cw() -> Vec<Coord<f64>> {
        vec![
            c(0.0, 0.0),
            c(0.0, 1.0),
            c(1.0, 1.0),
            c(1.0, 0.0),
            c(0.0, 0.0),
        ]
    }

    /// A single closed outer way should produce a single Polygon.
    #[test]
    fn single_outer_way_forms_polygon() {
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, square_ccw());
        let (geom, report) = assemble_multipolygon(&[1], &[], &way_geoms).unwrap();
        assert!(matches!(geom, Geometry::Polygon(_)));
        assert_eq!(report.outer_rings_built, 1);
        assert_eq!(report.inner_rings_built, 0);
        assert!(report.warnings.is_empty());
    }

    /// Two ways forming one ring should close correctly.
    #[test]
    fn two_ways_form_one_ring() {
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, vec![c(0.0, 0.0), c(1.0, 0.0), c(1.0, 1.0)]);
        way_geoms.insert(2, vec![c(1.0, 1.0), c(0.0, 1.0), c(0.0, 0.0)]);
        let (geom, report) = assemble_multipolygon(&[1, 2], &[], &way_geoms).unwrap();
        if let Geometry::Polygon(p) = geom {
            assert_eq!(p.exterior().0.len(), 5, "closed square must have 5 coords");
            assert!(p.exterior().is_ccw(), "must be CCW after orientation");
        } else {
            panic!("expected Polygon");
        }
        assert_eq!(report.outer_rings_built, 1);
    }

    /// A CW outer way must be flipped to CCW.
    #[test]
    fn cw_outer_is_flipped_to_ccw() {
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, square_cw());
        let (geom, _) = assemble_multipolygon(&[1], &[], &way_geoms).unwrap();
        if let Geometry::Polygon(p) = geom {
            assert!(p.exterior().is_ccw(), "CW input must be flipped to CCW");
        } else {
            panic!("expected Polygon");
        }
    }

    /// A hole that's inside the outer should be assigned as an interior
    /// and flipped to CW.
    #[test]
    fn inner_inside_outer_becomes_hole() {
        let outer = vec![
            c(0.0, 0.0),
            c(10.0, 0.0),
            c(10.0, 10.0),
            c(0.0, 10.0),
            c(0.0, 0.0),
        ];
        // CCW inner — will be flipped to CW
        let inner = vec![
            c(2.0, 2.0),
            c(3.0, 2.0),
            c(3.0, 3.0),
            c(2.0, 3.0),
            c(2.0, 2.0),
        ];
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, outer);
        way_geoms.insert(2, inner);

        let (geom, report) = assemble_multipolygon(&[1], &[2], &way_geoms).unwrap();
        if let Geometry::Polygon(p) = geom {
            assert_eq!(p.interiors().len(), 1);
            assert!(p.interiors()[0].is_cw(), "hole must be CW");
            assert!(p.exterior().is_ccw(), "exterior must be CCW");
        } else {
            panic!("expected Polygon with hole");
        }
        assert_eq!(report.inner_rings_matched, 1);
        assert!(report.warnings.is_empty());
    }

    /// An inner way that's outside any outer ring becomes an orphan warning.
    #[test]
    fn orphan_inner_is_warned() {
        let outer = vec![
            c(0.0, 0.0),
            c(1.0, 0.0),
            c(1.0, 1.0),
            c(0.0, 1.0),
            c(0.0, 0.0),
        ];
        // Way off to the side
        let orphan = vec![
            c(100.0, 100.0),
            c(101.0, 100.0),
            c(101.0, 101.0),
            c(100.0, 101.0),
            c(100.0, 100.0),
        ];
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, outer);
        way_geoms.insert(2, orphan);

        let (_, report) = assemble_multipolygon(&[1], &[2], &way_geoms).unwrap();
        assert_eq!(report.inner_rings_matched, 0);
        assert_eq!(report.warnings.len(), 1);
        assert!(matches!(
            report.warnings[0],
            AssemblyWarning::OrphanedInner { .. }
        ));
    }

    /// Two disjoint outer ways → MultiPolygon.
    #[test]
    fn multi_outer_becomes_multipolygon() {
        let outer1 = vec![
            c(0.0, 0.0),
            c(1.0, 0.0),
            c(1.0, 1.0),
            c(0.0, 1.0),
            c(0.0, 0.0),
        ];
        let outer2 = vec![
            c(5.0, 5.0),
            c(6.0, 5.0),
            c(6.0, 6.0),
            c(5.0, 6.0),
            c(5.0, 5.0),
        ];
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, outer1);
        way_geoms.insert(2, outer2);

        let (geom, report) = assemble_multipolygon(&[1, 2], &[], &way_geoms).unwrap();
        if let Geometry::MultiPolygon(mp) = geom {
            assert_eq!(mp.0.len(), 2);
            for p in &mp.0 {
                assert!(p.exterior().is_ccw());
            }
        } else {
            panic!("expected MultiPolygon");
        }
        assert_eq!(report.outer_rings_built, 2);
    }

    /// Out-of-order ways should still close (segment-oriented chaining
    /// handles any permutation of the input).
    #[test]
    fn out_of_order_ways_still_close() {
        // Build a square from four separate one-segment ways in reverse order
        let mut way_geoms = HashMap::new();
        way_geoms.insert(1, vec![c(0.0, 1.0), c(0.0, 0.0)]); // left edge
        way_geoms.insert(2, vec![c(1.0, 1.0), c(0.0, 1.0)]); // top edge
        way_geoms.insert(3, vec![c(1.0, 0.0), c(1.0, 1.0)]); // right edge
        way_geoms.insert(4, vec![c(0.0, 0.0), c(1.0, 0.0)]); // bottom edge

        let (geom, report) = assemble_multipolygon(&[1, 2, 3, 4], &[], &way_geoms).unwrap();
        assert!(matches!(geom, Geometry::Polygon(_)));
        assert_eq!(report.outer_rings_built, 1);
        if let Geometry::Polygon(p) = geom {
            assert_eq!(p.exterior().0.len(), 5);
            assert!(p.exterior().is_ccw());
        }
    }

    /// No outer rings → NoOuterRings error.
    #[test]
    fn no_outers_is_error() {
        let way_geoms = HashMap::new();
        let err = assemble_multipolygon(&[], &[], &way_geoms).unwrap_err();
        assert!(matches!(err, AssemblyError::NoOuterRings));
    }
}
