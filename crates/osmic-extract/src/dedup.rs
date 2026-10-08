//! Name + proximity deduplication for extracted entities.
//!
//! Entities are grouped by normalised name (Unicode NFKC, case-folded,
//! whitespace collapsed). Within a group, the richest entity of each
//! cluster is kept: entities are visited richest-first and dropped if a
//! kept entity lies within the radius (great-circle distance on a sphere).
//! Points are bucketed by their position on the unit sphere in cubes whose
//! edge is the chord the radius subtends, so any two points within the
//! radius are in the same or adjacent cubes: the neighbour search is exact
//! (also across the antimeridian and at the poles) and O(1) per entity, so
//! large chains (thousands of identically named locations) stay linear.
//! Entities without coordinates collapse to one per name. The result is
//! deterministic and sorted by element type and id.

use std::collections::{BTreeMap, HashMap};

use unicode_normalization::UnicodeNormalization;

use crate::entity::Entity;

/// Mean Earth radius (IUGG), in meters.
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// A WGS84 position as a point on the unit sphere.
fn unit_vector(lat: f64, lon: f64) -> [f64; 3] {
    let (sin_lat, cos_lat) = lat.to_radians().sin_cos();
    let (sin_lon, cos_lon) = lon.to_radians().sin_cos();
    [cos_lat * cos_lon, cos_lat * sin_lon, sin_lat]
}

/// Straight-line distance between two points on the unit sphere that are
/// `meters` apart along a great circle (monotonic in `meters` up to half
/// the circumference, beyond which every pair is within it).
fn chord_for(meters: f64) -> f64 {
    let angle = (meters / EARTH_RADIUS_M).min(std::f64::consts::PI);
    2.0 * (angle / 2.0).sin()
}

fn chord_squared(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| (a[i] - b[i]).powi(2)).sum()
}

/// Normalised grouping key for a name.
pub fn normalize_name(name: &str) -> String {
    name.nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Deduplicate entities by normalised name and proximity.
///
/// Non-finite or non-positive radii disable deduplication (entities are
/// returned unchanged apart from ordering).
pub fn deduplicate(entities: Vec<Entity>, radius_meters: f64) -> Vec<Entity> {
    let mut out: Vec<Entity> = if !(radius_meters.is_finite() && radius_meters > 0.0) {
        entities
    } else {
        let mut groups: BTreeMap<String, Vec<Entity>> = BTreeMap::new();
        for e in entities {
            groups.entry(normalize_name(&e.name)).or_default().push(e);
        }
        groups
            .into_values()
            .flat_map(|g| dedup_group(g, radius_meters))
            .collect()
    };
    out.sort_by_key(Entity::sort_key);
    out
}

fn dedup_group(mut group: Vec<Entity>, radius: f64) -> Vec<Entity> {
    // Richest first; ties broken by element for determinism.
    group.sort_by(|a, b| {
        b.richness()
            .cmp(&a.richness())
            .then_with(|| a.sort_key().cmp(&b.sort_key()))
    });
    // Two points within the radius are less than `chord` apart in every
    // coordinate, so they lie in the same or adjacent cubes of edge `chord`.
    let chord = chord_for(radius);
    let chord_sq = chord * chord;
    // Saturating casts keep absurdly small radii correct (just slower).
    let cell_of = |p: [f64; 3]| p.map(|v| (v / chord).floor() as i64);
    let mut grid: HashMap<[i64; 3], Vec<[f64; 3]>> = HashMap::new();
    let mut kept: Vec<Entity> = Vec::new();
    let mut kept_unlocated = false;
    for e in group {
        let (Some(lat), Some(lon)) = (e.lat, e.lon) else {
            if !kept_unlocated {
                kept_unlocated = true;
                kept.push(e);
            }
            continue;
        };
        let p = unit_vector(lat, lon);
        let [x, y, z] = cell_of(p);
        let step = |a: i64, d: i64| a.saturating_add(d);
        let mut near = (-1..=1).flat_map(|dx| {
            (-1..=1)
                .flat_map(move |dy| (-1..=1).map(move |dz| [step(x, dx), step(y, dy), step(z, dz)]))
        });
        let duplicate = near.any(|c| {
            grid.get(&c)
                .is_some_and(|points| points.iter().any(|&q| chord_squared(p, q) < chord_sq))
        });
        if !duplicate {
            grid.entry([x, y, z]).or_default().push(p);
            kept.push(e);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use osmic_core::OsmType;

    fn entity(id: i64, name: &str, at: Option<(f64, f64)>, phone: &str) -> Entity {
        let mut tags = vec![("name", name)];
        if !phone.is_empty() {
            tags.push(("phone", phone));
        }
        Entity::new(
            OsmType::Node,
            id,
            at.map(|(lat, lon)| geo_types::Coord { x: lon, y: lat }),
            &tags,
        )
    }

    const METERS_PER_DEGREE: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;

    /// Haversine distance in meters, independent of the chord arithmetic
    /// under test.
    fn haversine(a: (f64, f64), b: (f64, f64)) -> f64 {
        let (phi1, phi2) = (a.0.to_radians(), b.0.to_radians());
        let dphi = (b.0 - a.0).to_radians();
        let dlambda = (b.1 - a.1).to_radians();
        let h =
            (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
        EARTH_RADIUS_M * 2.0 * h.sqrt().atan2((1.0 - h).sqrt())
    }

    /// The point `meters` from `from` along initial `bearing` (degrees),
    /// with longitude wrapped to [-180, 180).
    fn destination(from: (f64, f64), bearing: f64, meters: f64) -> (f64, f64) {
        let (phi1, lambda1) = (from.0.to_radians(), from.1.to_radians());
        let (delta, theta) = (meters / EARTH_RADIUS_M, bearing.to_radians());
        let phi2 = (phi1.sin() * delta.cos() + phi1.cos() * delta.sin() * theta.cos()).asin();
        let lambda2 = lambda1
            + (theta.sin() * delta.sin() * phi1.cos()).atan2(delta.cos() - phi1.sin() * phi2.sin());
        let lon = (lambda2.to_degrees() + 540.0).rem_euclid(360.0) - 180.0;
        (phi2.to_degrees(), lon)
    }

    /// SplitMix64: a reproducible stream of uniform floats in [0, 1).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn range(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * self.next()
        }
    }

    /// Pairs of identically named points `lo..hi` meters apart around
    /// places where a planar grid goes wrong: far from the prime meridian,
    /// across the antimeridian, near and at the poles, and anywhere at all.
    fn pairs(lo: f64, hi: f64) -> Vec<Entity> {
        let mut rng = Rng(42);
        let mut centres = vec![
            (37.77, -122.42), // San Francisco
            (35.68, 139.69),  // Tokyo
            (-33.87, 151.21), // Sydney
            (0.0, 180.0),
            (64.84, -179.999_9),
            (-46.0, 179.999),
            (78.22, 15.65),
            (89.999, 0.0),
            (90.0, 0.0),
            (-90.0, 0.0),
        ];
        for _ in 0..30 {
            centres.push((rng.range(-90.0, 90.0), rng.range(-180.0, 180.0)));
        }
        let mut out = Vec::new();
        for centre in centres {
            for _ in 0..400 {
                let a = destination(centre, rng.range(0.0, 360.0), rng.range(0.0, 2_000.0));
                let b = destination(a, rng.range(0.0, 360.0), rng.range(lo, hi));
                let id = out.len() as i64;
                let name = format!("pair {id}");
                out.push(entity(id, &name, Some(a), ""));
                out.push(entity(id + 1, &name, Some(b), ""));
            }
        }
        out
    }

    #[test]
    fn every_pair_within_the_radius_merges() {
        let input = pairs(0.0, 99.0);
        for p in input.chunks(2) {
            let d = haversine(
                (p[0].lat.unwrap_or(0.0), p[0].lon.unwrap_or(0.0)),
                (p[1].lat.unwrap_or(0.0), p[1].lon.unwrap_or(0.0)),
            );
            assert!(d < 99.5, "fixture pair is {d} m apart");
        }
        let pairs = input.len() / 2;
        let kept = deduplicate(input, 100.0).len();
        assert_eq!(
            kept,
            pairs,
            "{} of {pairs} pairs within 100 m left unmerged",
            kept.saturating_sub(pairs)
        );
    }

    #[test]
    fn no_pair_beyond_the_radius_merges() {
        let input = pairs(101.0, 400.0);
        let n = input.len();
        assert_eq!(deduplicate(input, 100.0).len(), n);
    }

    #[test]
    fn merges_across_the_antimeridian_and_over_the_pole() {
        let out = deduplicate(
            vec![
                entity(1, "X", Some((10.0, 179.9998)), ""),
                entity(2, "X", Some((10.0, -179.9998)), ""),
                entity(3, "Y", Some((89.9997, 0.0)), ""),
                entity(4, "Y", Some((89.9997, 180.0)), ""),
            ],
            100.0,
        );
        assert_eq!(out.len(), 2, "{out:?}");
    }

    #[test]
    fn same_name_same_place_keeps_richest() {
        let out = deduplicate(
            vec![
                entity(1, "Acme Corp", Some((25.7, -80.2)), ""),
                entity(2, "ACME  corp", Some((25.7001, -80.2)), "555-1234"),
            ],
            100.0,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].phone, "555-1234");
    }

    #[test]
    fn same_name_far_apart_are_kept() {
        let out = deduplicate(
            vec![
                entity(1, "Acme", Some((25.7, -80.2)), ""),
                entity(2, "Acme", Some((40.7, -74.0)), ""),
            ],
            100.0,
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn unicode_normalisation_groups_equivalent_names() {
        // Precomposed vs combining-accent "Café", and a full-width "Ｃafé".
        assert_eq!(normalize_name("Café"), normalize_name("Cafe\u{301}"));
        assert_eq!(normalize_name("Ｃafé  Bar"), "café bar");
    }

    #[test]
    fn unlocated_collapse_per_name_but_not_with_located() {
        let out = deduplicate(
            vec![
                entity(1, "Acme", None, ""),
                entity(2, "Acme", None, "555"),
                entity(3, "Acme", Some((1.0, 1.0)), ""),
            ],
            100.0,
        );
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|e| e.lat.is_none() && e.phone == "555"));
    }

    #[test]
    fn neighbouring_cells_are_checked_near_high_latitudes() {
        // Two points 50 m apart east-west at 70°N straddling a cell edge.
        let lat: f64 = 70.0;
        let dlon = 50.0 / (METERS_PER_DEGREE * lat.to_radians().cos());
        let out = deduplicate(
            vec![
                entity(1, "X", Some((lat, 10.0)), ""),
                entity(2, "X", Some((lat, 10.0 + dlon)), ""),
            ],
            100.0,
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn large_chain_matches_brute_force_and_is_deterministic() {
        // 20,000 identically named locations on a 60 m grid at 60°N that
        // straddles the antimeridian: orthogonal and diagonal neighbours
        // (60 m, 85 m) are within 100 m, the next ring (120 m) is not.
        const ROWS: i32 = 100;
        const COLS: i32 = 200;
        let lat0: f64 = 60.0;
        let dlat = 60.0 / METERS_PER_DEGREE;
        let dlon = 60.0 / (METERS_PER_DEGREE * lat0.to_radians().cos());
        let make = || {
            (0..ROWS * COLS)
                .map(|i| {
                    let (row, col) = (f64::from(i / COLS), f64::from(i % COLS - COLS / 2));
                    let lon = (180.0 + col * dlon + 540.0).rem_euclid(360.0) - 180.0;
                    entity(
                        i64::from(i),
                        "Big Chain",
                        Some((lat0 + row * dlat, lon)),
                        "",
                    )
                })
                .collect::<Vec<_>>()
        };

        let fast = deduplicate(make(), 100.0);

        // Reference: the same greedy rule (equal richness, so ascending id)
        // checking every kept entity.
        let mut reference: Vec<Entity> = Vec::new();
        for e in make() {
            let p = (e.lat.unwrap_or(0.0), e.lon.unwrap_or(0.0));
            let near = reference.iter().any(|k| {
                let q = (k.lat.unwrap_or(0.0), k.lon.unwrap_or(0.0));
                (p.0 - q.0).abs() < 0.01 && haversine(p, q) < 100.0
            });
            if !near {
                reference.push(e);
            }
        }
        assert_eq!(fast, reference);
        assert!(
            fast.len() < make().len() / 3,
            "neighbours merged: {} kept",
            fast.len()
        );

        let mut reversed = make();
        reversed.reverse();
        assert_eq!(
            deduplicate(reversed, 100.0),
            fast,
            "input order does not change the output"
        );
    }

    #[test]
    fn disabled_radius_keeps_everything() {
        let e = vec![
            entity(1, "A", Some((0.0, 0.0)), ""),
            entity(2, "A", Some((0.0, 0.0)), ""),
        ];
        assert_eq!(deduplicate(e.clone(), 0.0).len(), 2);
        assert_eq!(deduplicate(e, f64::NAN).len(), 2);
    }
}
