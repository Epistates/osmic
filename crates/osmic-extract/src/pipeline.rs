//! Two-pass PBF extraction pipeline.
//!
//! Unlike `osmic-osm::PbfProcessor` which classifies features for map rendering,
//! this pipeline extracts arbitrary named entities matching tag filters and
//! collects business contact metadata.

use std::path::Path;
use std::time::{Duration, Instant};

use osmic_core::error::{OsmicError, OsmicResult};
use osmic_core::{LonLat, NodeLocationStore};
use osmic_index::RamNodeLocationStore;
use osmpbf::{Element, ElementReader};
use tracing::info;

use crate::entity::Entity;
use crate::filter::TagFilter;

/// Configuration for the extraction pipeline.
#[derive(Debug, Clone)]
pub struct ExtractConfig {
    /// Tag filter to match entities.
    pub filter: TagFilter,
    /// Only extract entities that have a `name` tag.
    pub require_name: bool,
    /// Maximum expected node ID (determines mmap file size).
    /// For 2025 North America: ~13_000_000_000.
    pub max_node_id: i64,
    /// Optional bounding box filter: (min_lon, min_lat, max_lon, max_lat).
    pub bbox: Option<(f64, f64, f64, f64)>,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            filter: TagFilter::All(vec![]),
            require_name: true,
            max_node_id: 13_000_000_000,
            bbox: None,
        }
    }
}

/// Result of running the extraction pipeline.
#[derive(Debug)]
pub struct ExtractResult {
    pub entities: Vec<Entity>,
    pub stats: ExtractStats,
}

/// Statistics from the extraction.
#[derive(Debug, Clone)]
pub struct ExtractStats {
    pub node_count: u64,
    pub way_count: u64,
    pub relation_count: u64,
    pub matched_count: u64,
    pub pass1_duration: Duration,
    pub pass2_duration: Duration,
    pub total_duration: Duration,
}

/// Two-pass PBF extractor for business entities.
pub struct Extractor {
    config: ExtractConfig,
}

impl Extractor {
    pub fn new(config: ExtractConfig) -> Self {
        Self { config }
    }

    /// Run the extraction pipeline on a PBF file.
    pub fn extract(&self, pbf_path: &Path) -> OsmicResult<ExtractResult> {
        let total_start = Instant::now();

        if !pbf_path.exists() {
            return Err(OsmicError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("PBF file not found: {}", pbf_path.display()),
            )));
        }

        let node_store = RamNodeLocationStore::create(self.config.max_node_id)?;

        // Pass 1: Read node locations
        info!("Pass 1: Reading node locations...");
        let pass1_start = Instant::now();
        let node_count = self.pass1_nodes(pbf_path, &node_store)?;
        let pass1_duration = pass1_start.elapsed();
        info!(
            "Pass 1 complete: {} nodes in {:.2}s ({:.0} nodes/s)",
            node_count,
            pass1_duration.as_secs_f64(),
            node_count as f64 / pass1_duration.as_secs_f64().max(0.001)
        );

        // Pass 2: Extract matching entities
        info!("Pass 2: Extracting matching entities...");
        let pass2_start = Instant::now();
        let (entities, way_count, relation_count) = self.pass2_extract(pbf_path, &node_store)?;
        let pass2_duration = pass2_start.elapsed();
        let matched_count = entities.len() as u64;
        info!(
            "Pass 2 complete: {} entities matched in {:.2}s",
            matched_count,
            pass2_duration.as_secs_f64()
        );

        let total_duration = total_start.elapsed();
        drop(node_store);

        Ok(ExtractResult {
            entities,
            stats: ExtractStats {
                node_count,
                way_count,
                relation_count,
                matched_count,
                pass1_duration,
                pass2_duration,
                total_duration,
            },
        })
    }

    /// Pass 1: Store all node locations in the mmap'd node store.
    fn pass1_nodes(&self, pbf_path: &Path, node_store: &RamNodeLocationStore) -> OsmicResult<u64> {
        let reader =
            ElementReader::from_path(pbf_path).map_err(|e| OsmicError::Pbf(e.to_string()))?;

        reader
            .par_map_reduce(
                |element| match element {
                    Element::Node(node) => {
                        node_store.set(node.id(), node.lon(), node.lat());
                        1u64
                    }
                    Element::DenseNode(node) => {
                        node_store.set(node.id(), node.lon(), node.lat());
                        1u64
                    }
                    _ => 0,
                },
                || 0u64,
                |a, b| a + b,
            )
            .map_err(|e| OsmicError::Pbf(e.to_string()))
    }

    /// Pass 2: Process all elements, filter by tags, extract entities.
    fn pass2_extract(
        &self,
        pbf_path: &Path,
        node_store: &RamNodeLocationStore,
    ) -> OsmicResult<(Vec<Entity>, u64, u64)> {
        let reader =
            ElementReader::from_path(pbf_path).map_err(|e| OsmicError::Pbf(e.to_string()))?;
        let filter = &self.config.filter;
        let require_name = self.config.require_name;
        let bbox = self.config.bbox;

        let (entities, way_count, relation_count) = reader
            .par_map_reduce(
                |element| {
                    let mut local_entities: Vec<Entity> = Vec::new();
                    let mut local_way_count = 0u64;
                    let mut local_rel_count = 0u64;

                    match element {
                        Element::Node(node) => {
                            let tags: Vec<(String, String)> = node
                                .tags()
                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                .collect();

                            if let Some(entity) =
                                try_build_entity(&tags, filter, require_name, bbox, || {
                                    ("node", node.id(), Some(node.lon()), Some(node.lat()))
                                })
                            {
                                local_entities.push(entity);
                            }
                        }
                        Element::DenseNode(node) => {
                            let tags: Vec<(String, String)> = node
                                .tags()
                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                .collect();

                            if let Some(entity) =
                                try_build_entity(&tags, filter, require_name, bbox, || {
                                    ("node", node.id(), Some(node.lon()), Some(node.lat()))
                                })
                            {
                                local_entities.push(entity);
                            }
                        }
                        Element::Way(way) => {
                            local_way_count = 1;
                            let tags: Vec<(String, String)> = way
                                .tags()
                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                .collect();

                            if let Some(entity) =
                                try_build_entity(&tags, filter, require_name, bbox, || {
                                    let refs: Vec<i64> = way.refs().collect();
                                    let coords: Vec<LonLat> =
                                        refs.iter().filter_map(|&id| node_store.get(id)).collect();
                                    let (lat, lon) = if coords.is_empty() {
                                        (None, None)
                                    } else {
                                        let lat = coords.iter().map(|c| c.lat).sum::<f64>()
                                            / coords.len() as f64;
                                        let lon = coords.iter().map(|c| c.lon).sum::<f64>()
                                            / coords.len() as f64;
                                        (Some(lat), Some(lon))
                                    };
                                    ("way", way.id(), lon, lat)
                                })
                            {
                                local_entities.push(entity);
                            }
                        }
                        Element::Relation(rel) => {
                            local_rel_count = 1;
                            let tags: Vec<(String, String)> = rel
                                .tags()
                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                .collect();

                            if let Some(entity) =
                                try_build_entity(&tags, filter, require_name, bbox, || {
                                    ("relation", rel.id(), None, None)
                                })
                            {
                                local_entities.push(entity);
                            }
                        }
                    }

                    (local_entities, local_way_count, local_rel_count)
                },
                || (Vec::new(), 0u64, 0u64),
                |(mut ea, wa, ra), (eb, wb, rb)| {
                    ea.extend(eb);
                    (ea, wa + wb, ra + rb)
                },
            )
            .map_err(|e| OsmicError::Pbf(e.to_string()))?;

        Ok((entities, way_count, relation_count))
    }
}

/// Try to build an Entity from a set of tags, applying filter and name requirement.
/// The `coords_fn` is only called if the filter matches (lazy coordinate resolution).
fn try_build_entity<F>(
    tags: &[(String, String)],
    filter: &TagFilter,
    require_name: bool,
    bbox: Option<(f64, f64, f64, f64)>,
    coords_fn: F,
) -> Option<Entity>
where
    F: FnOnce() -> (&'static str, i64, Option<f64>, Option<f64>),
{
    // Check name requirement first (cheap)
    let name = tags
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.as_str());

    if require_name && name.is_none() {
        return None;
    }

    // Apply tag filter
    if !filter.matches(tags) {
        return None;
    }

    // Resolve coordinates (potentially expensive for ways)
    let (osm_type, osm_id, lon, lat) = coords_fn();

    // Apply bbox filter if set
    if let Some(bbox) = bbox {
        match (lon, lat) {
            (Some(lon), Some(lat)) => {
                let (min_lon, min_lat, max_lon, max_lat) = bbox;
                if lon < min_lon || lon > max_lon || lat < min_lat || lat > max_lat {
                    return None;
                }
            }
            // No coordinates and bbox is set — exclude (can't verify location)
            _ => return None,
        }
    }

    let operator = tags
        .iter()
        .find(|(k, _)| k == "operator")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    Some(Entity {
        name: name.unwrap_or("").to_string(),
        osm_type: osm_type.to_string(),
        osm_id,
        lat,
        lon,
        address: Entity::build_address(tags),
        phone: Entity::extract_phone(tags),
        website: Entity::extract_website(tags),
        operator,
        tags: Entity::format_tags(tags),
        address_parts: Entity::build_address_parts(tags),
    })
}
