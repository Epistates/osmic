//! Two-pass extraction pipeline.
//!
//! Pass 1 ([`osmic_osm::scan_nodes`]) builds the node index and collects the
//! relations that match the filter. Pass 2 matches nodes and ways — tags are
//! compared as borrowed strings, so nothing is allocated for the vast
//! majority of elements that do not match — and caches the coordinates of
//! ways the matched relations reference. Relations are located last.
//!
//! Entity locations:
//! - **nodes**: the node itself;
//! - **closed ways**: a point guaranteed inside the polygon (not the
//!   centroid, which can fall outside concave shapes);
//! - **open ways**: the midpoint along the line;
//! - **relations**: an interior point of the assembled area for
//!   multipolygon/boundary relations, otherwise the centroid of all member
//!   coordinates; `None` if no member is in the input.

use std::path::Path;
use std::time::{Duration, Instant};

use geo::{Centroid, Euclidean, InteriorPoint, InterpolatableLine};
use geo_types::{Coord, LineString, MultiPoint, Point, Polygon};
use osmpbf::Element;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::info;

use osmic_core::{BBox, FixedCoord, Geometry, NodeLocationStore, OsmType};
use osmic_index::NodeIndex;
use osmic_osm::multipolygon::{MemberWay, Role, assemble_area};
use osmic_osm::pbf::{StringTable, par_blocks};
use osmic_osm::{NodeStorage, OsmError, RelationRecord, TagFilter, scan_nodes};

use crate::entity::Entity;

/// Extraction settings. Start from [`Default`] and set the fields you need.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExtractConfig {
    /// Entities must match this filter (default: everything).
    pub filter: TagFilter,
    /// Only extract entities with a non-empty `name` tag (default: true).
    pub require_name: bool,
    /// Keep only entities located inside this box (default: no limit).
    pub bbox: Option<BBox>,
    /// Where node locations are kept while ways are located (default:
    /// sparse in-memory index). Unused for files with locations on ways.
    pub node_storage: NodeStorage,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            filter: TagFilter::everything(),
            require_name: true,
            bbox: None,
            node_storage: NodeStorage::Sparse,
        }
    }
}

/// Result of an extraction.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExtractResult {
    /// Matched entities, sorted by element type and id.
    pub entities: Vec<Entity>,
    /// Counts and timings.
    pub stats: ExtractStats,
}

/// Statistics from an extraction.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExtractStats {
    /// Nodes in the input.
    pub node_count: u64,
    /// Ways in the input.
    pub way_count: u64,
    /// Relations in the input.
    pub relation_count: u64,
    /// Entities extracted (before deduplication).
    pub matched_count: u64,
    /// Matched entities without a location (no member in the input).
    pub unlocated: u64,
    /// Time spent indexing node locations and collecting relations.
    pub pass1_duration: Duration,
    /// Time spent matching nodes and ways and locating relations.
    pub pass2_duration: Duration,
    /// Wall time of the whole extraction.
    pub total_duration: Duration,
}

/// Extracts named entities from a PBF file.
pub struct Extractor {
    config: ExtractConfig,
}

fn has_name(tags: &[(&str, &str)]) -> bool {
    tags.iter().any(|(k, v)| *k == "name" && !v.is_empty())
}

/// Append the valid locations of a way's nodes (from a LocationsOnWays
/// file) to `coords`; returns whether every node had one. Writers such as
/// osmium store a node missing from their input as an out-of-range
/// coordinate (`i32::MAX`), which must not be taken for a location.
fn push_locations(
    coords: &mut Vec<FixedCoord>,
    locations: impl Iterator<Item = FixedCoord>,
) -> bool {
    let mut complete = true;
    for c in locations {
        if c.is_valid() {
            coords.push(c);
        } else {
            complete = false;
        }
    }
    complete
}

/// A node's location, unless its coordinate is out of range.
fn node_location(c: FixedCoord) -> Option<Coord<f64>> {
    c.is_valid().then(|| c.to_coord())
}

/// A representative point for a way.
fn way_location(coords: &[FixedCoord], closed: bool) -> Option<Coord<f64>> {
    let line = LineString(coords.iter().map(|c| c.to_coord()).collect());
    if closed
        && coords.len() >= 4
        && let Some(p) = Polygon::new(line.clone(), vec![]).interior_point()
    {
        return Some(p.0);
    }
    match coords.len() {
        0 => None,
        1 => Some(coords[0].to_coord()),
        _ => line.point_at_ratio_from_start(&Euclidean, 0.5).map(|p| p.0),
    }
}

fn geometry_point(g: &Geometry) -> Option<Coord<f64>> {
    match g {
        Geometry::Polygon(p) => p.interior_point().map(|p| p.0),
        Geometry::MultiPolygon(mp) => mp.interior_point().map(|p| p.0),
        other => other
            .bbox()
            .is_valid()
            .then(|| other.bbox().center().into()),
    }
}

struct Pass2Block {
    entities: Vec<Entity>,
    cached: Vec<(i64, Vec<FixedCoord>)>,
}

impl Extractor {
    /// An extractor with the given settings.
    pub fn new(config: ExtractConfig) -> Self {
        Self { config }
    }

    fn wanted(&self, tags: &[(&str, &str)]) -> bool {
        (!self.config.require_name || has_name(tags)) && self.config.filter.matches(tags)
    }

    fn entity(
        &self,
        osm_type: OsmType,
        id: i64,
        tags: &[(&str, &str)],
        location: Option<Coord<f64>>,
    ) -> Option<Entity> {
        if let Some(bbox) = &self.config.bbox {
            // Without a location the entity cannot be shown to be inside.
            let c = location?;
            if !bbox.contains_point(c.x, c.y) {
                return None;
            }
        }
        Some(Entity::new(osm_type, id, location, tags))
    }

    /// Run the extraction.
    pub fn extract(&self, path: &Path) -> Result<ExtractResult, OsmError> {
        let start = Instant::now();
        let select = |tags: &[(&str, &str)]| self.wanted(tags);
        let scan = scan_nodes(path, &self.config.node_storage, &select)?;
        let index = scan.index;
        let relations = scan.relations;
        let needed_ways: FxHashSet<i64> = relations
            .iter()
            .flat_map(|r| &r.members)
            .filter(|m| m.osm_type == OsmType::Way)
            .map(|m| m.id)
            .collect();

        info!("Pass 2: matching nodes and ways");
        let pass2_start = Instant::now();
        let blocks = par_blocks(path, |block| {
            let mut out = Pass2Block {
                entities: Vec::new(),
                cached: Vec::new(),
            };
            let strings = StringTable::new(block);
            let mut tags: Vec<(&str, &str)> = Vec::new();
            let mut coords: Vec<FixedCoord> = Vec::new();
            for element in block.elements() {
                match element {
                    Element::DenseNode(n) => {
                        tags.clear();
                        tags.extend(strings.tags(n.raw_tags()));
                        if !tags.is_empty() && self.wanted(&tags) {
                            let c = FixedCoord::new(n.decimicro_lon(), n.decimicro_lat());
                            out.entities.extend(self.entity(
                                OsmType::Node,
                                n.id(),
                                &tags,
                                node_location(c),
                            ));
                        }
                    }
                    Element::Node(n) => {
                        tags.clear();
                        tags.extend(strings.tags(n.raw_tags()));
                        if !tags.is_empty() && self.wanted(&tags) {
                            let c = FixedCoord::new(n.decimicro_lon(), n.decimicro_lat());
                            out.entities.extend(self.entity(
                                OsmType::Node,
                                n.id(),
                                &tags,
                                node_location(c),
                            ));
                        }
                    }
                    Element::Way(w) => {
                        tags.clear();
                        tags.extend(strings.tags(w.raw_tags()));
                        let matched = !tags.is_empty() && self.wanted(&tags);
                        let needed = needed_ways.contains(&w.id());
                        if !matched && !needed {
                            continue;
                        }
                        coords.clear();
                        let mut first = None;
                        let mut last = None;
                        let mut complete = true;
                        for r in w.refs() {
                            first.get_or_insert(r);
                            last = Some(r);
                            match index.as_ref().and_then(|i| i.get(r)) {
                                Some(c) => coords.push(c),
                                None => complete = false,
                            }
                        }
                        if index.is_none() {
                            complete = push_locations(
                                &mut coords,
                                w.node_locations()
                                    .map(|l| FixedCoord::new(l.decimicro_lon(), l.decimicro_lat())),
                            );
                        }
                        if needed && complete {
                            out.cached.push((w.id(), coords.clone()));
                        }
                        if matched {
                            let closed = complete && first == last && coords.len() >= 4;
                            let location = way_location(&coords, closed);
                            out.entities
                                .extend(self.entity(OsmType::Way, w.id(), &tags, location));
                        }
                    }
                    Element::Relation(_) => {}
                }
            }
            Ok(out)
        })?;
        let mut entities = Vec::new();
        let mut way_cache: FxHashMap<i64, Vec<FixedCoord>> = FxHashMap::default();
        for (_, b) in blocks {
            entities.extend(b.entities);
            way_cache.extend(b.cached);
        }

        for rel in &relations {
            let location = relation_location(rel, &way_cache, index.as_ref());
            let tags: Vec<(&str, &str)> = rel
                .tags
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            entities.extend(self.entity(OsmType::Relation, rel.id, &tags, location));
        }
        entities.sort_by_key(Entity::sort_key);

        let pass2_duration = pass2_start.elapsed();
        let stats = ExtractStats {
            node_count: scan.node_count,
            way_count: scan.way_count,
            relation_count: scan.relation_count,
            matched_count: entities.len() as u64,
            unlocated: entities.iter().filter(|e| e.lat.is_none()).count() as u64,
            pass1_duration: scan.duration,
            pass2_duration,
            total_duration: start.elapsed(),
        };
        info!(
            matched = stats.matched_count,
            secs = stats.total_duration.as_secs_f64(),
            "Extraction complete"
        );
        Ok(ExtractResult { entities, stats })
    }
}

/// Locate a relation from its members.
fn relation_location(
    rel: &RelationRecord,
    ways: &FxHashMap<i64, Vec<FixedCoord>>,
    nodes: Option<&NodeIndex>,
) -> Option<Coord<f64>> {
    let is_area = rel
        .tags
        .iter()
        .any(|(k, v)| k == "type" && (v == "multipolygon" || v == "boundary"));
    if is_area {
        let members: Vec<MemberWay<'_>> = rel
            .members
            .iter()
            .filter(|m| m.osm_type == OsmType::Way)
            .filter_map(|m| {
                Some(MemberWay {
                    id: m.id,
                    role: Role::parse(&m.role)?,
                    coords: ways.get(&m.id)?,
                })
            })
            .collect();
        if let Ok((geometry, _)) = assemble_area(&members)
            && let Some(c) = geometry_point(&geometry)
        {
            return Some(c);
        }
    }
    // Fallback: centroid of every member coordinate we have.
    let mut points: Vec<Point<f64>> = Vec::new();
    for m in &rel.members {
        match m.osm_type {
            OsmType::Node => {
                if let Some(c) = nodes.and_then(|n| n.get(m.id)) {
                    points.push(Point(c.to_coord()));
                }
            }
            OsmType::Way => {
                if let Some(cs) = ways.get(&m.id) {
                    points.extend(cs.iter().map(|c| Point(c.to_coord())));
                }
            }
            OsmType::Relation => {}
        }
    }
    MultiPoint(points).centroid().map(|p| p.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(lon: i32, lat: i32) -> FixedCoord {
        FixedCoord::new(lon, lat)
    }

    #[test]
    fn closed_way_location_is_inside_concave_polygon() {
        // A "C" shape whose centroid lies outside it.
        let c = vec![
            f(0, 0),
            f(100, 0),
            f(100, 10),
            f(10, 10),
            f(10, 90),
            f(100, 90),
            f(100, 100),
            f(0, 100),
            f(0, 0),
        ];
        let p = way_location(&c, true).expect("location");
        let poly = Polygon::new(LineString(c.iter().map(|x| x.to_coord()).collect()), vec![]);
        use geo::Contains;
        assert!(poly.contains(&Point(p)), "{p:?} not inside");
    }

    #[test]
    fn missing_locations_on_ways_are_not_coordinates() {
        // osmium's encoding of "node not in the input" on a way.
        let missing = f(i32::MAX, i32::MAX);
        let mut coords = Vec::new();
        assert!(!push_locations(
            &mut coords,
            [f(0, 0), missing, f(10, 10)].into_iter()
        ));
        assert_eq!(coords, [f(0, 0), f(10, 10)]);
        let location = way_location(&coords, false).expect("location");
        assert!(
            location.x.abs() < 1e-5 && location.y.abs() < 1e-5,
            "{location:?}"
        );

        coords.clear();
        assert!(push_locations(
            &mut coords,
            [f(0, 0), f(10, 10)].into_iter()
        ));
        coords.clear();
        assert!(!push_locations(&mut coords, [missing].into_iter()));
        assert_eq!(way_location(&coords, false), None);
        assert_eq!(node_location(missing), None);
        assert!(node_location(f(-1_800_000_000, 900_000_000)).is_some());
    }

    #[test]
    fn open_way_location_is_midpoint_along_line() {
        let line = vec![f(0, 0), f(1_000, 0), f(1_000, 1_000)];
        let p = way_location(&line, false).expect("location");
        assert!((p.x - 1_000e-7).abs() < 1e-12 && p.y.abs() < 1e-12, "{p:?}");
    }
}
