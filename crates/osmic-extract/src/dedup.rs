//! Name + proximity deduplication for extracted entities.
//!
//! Entities are grouped by normalised name (Unicode NFKC, case-folded,
//! whitespace collapsed). Within a group, the richest entity of each
//! cluster is kept: entities are visited richest-first and dropped if a
//! kept entity lies within the radius. A grid of radius-sized cells makes
//! the neighbour search O(1) per entity, so large chains (thousands of
//! identically named locations) stay linear. Entities without coordinates
//! collapse to one per name. The result is deterministic and sorted by
//! element type and id.

use std::collections::{BTreeMap, HashMap};

use unicode_normalization::UnicodeNormalization;

use crate::entity::Entity;

const EARTH_RADIUS_M: f64 = 6_371_000.0;
const METERS_PER_DEGREE: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;

/// Haversine distance between two WGS84 points in meters.
fn haversine_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (phi1, phi2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    EARTH_RADIUS_M * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
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
    let cell_deg = radius / METERS_PER_DEGREE;
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
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
        // Longitude cells shrink with latitude; scale so a cell spans at
        // least `radius` meters east-west.
        let lon_scale = lat.to_radians().cos().max(1e-6);
        let cell = (
            (lat / cell_deg).floor() as i64,
            (lon * lon_scale / cell_deg).floor() as i64,
        );
        let near = (-1..=1).flat_map(|dy| (-1..=1).map(move |dx| (cell.0 + dy, cell.1 + dx)));
        let duplicate = near.filter_map(|c| grid.get(&c)).flatten().any(|&i| {
            match (kept[i].lat, kept[i].lon) {
                (Some(klat), Some(klon)) => haversine_meters(lat, lon, klat, klon) < radius,
                _ => false,
            }
        });
        if !duplicate {
            grid.entry(cell).or_default().push(kept.len());
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
    fn large_chain_is_linear_and_deterministic() {
        let make = || {
            (0..20_000)
                .map(|i| {
                    entity(
                        i,
                        "Big Chain",
                        Some((
                            f64::from(i as i32 % 200) * 0.01,
                            f64::from(i as i32 / 200) * 0.01,
                        )),
                        "",
                    )
                })
                .collect::<Vec<_>>()
        };
        let a = deduplicate(make(), 100.0);
        let mut reversed = make();
        reversed.reverse();
        let b = deduplicate(reversed, 100.0);
        assert_eq!(a.len(), 20_000, "points are ~1 km apart");
        assert_eq!(a, b, "input order does not change the output");
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
