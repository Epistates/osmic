//! PBF parsing pipeline — native only (requires osmpbf + rayon).
#![cfg(feature = "native")]

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Query available system memory in bytes.
use geo_types::{Coord, LineString, Polygon};
use osmpbf::{Element, ElementReader, RelMemberType};
use tracing::info;

pub use osmic_core::NodeLocationStore;
use osmic_core::bbox::BBox;
use osmic_core::error::{OsmicError, OsmicResult};
use osmic_core::geometry::Geometry;

use crate::classify;
use crate::feature::Feature;
use crate::layers::LayerSet;
use crate::tags::{TagStore, Tags};

/// Statistics from PBF processing.
#[derive(Debug, Clone)]
pub struct PipelineStats {
    pub node_count: u64,
    pub way_count: u64,
    pub relation_count: u64,
    pub feature_count: u64,
    pub pass1_duration: Duration,
    pub pass2_duration: Duration,
    pub total_duration: Duration,
}

/// Result of processing a PBF file.
pub struct ProcessedData {
    pub tag_store: Arc<TagStore>,
    pub features: Vec<Feature>,
    pub bbox: BBox,
    pub stats: PipelineStats,
}

/// Three-pass PBF processor.
///
/// - Pass 1: Node locations → DenseNodeLocationStore (mmap)
/// - Pass 2: Ways → classify by tags → resolve node coords → emit Features
/// - Pass 3: Relations → assemble multipolygons (future)
pub struct PbfProcessor {
    pub tag_store: Arc<TagStore>,
}

impl PbfProcessor {
    pub fn new() -> Self {
        Self {
            tag_store: Arc::new(TagStore::new()),
        }
    }

    /// Process a PBF file using the provided node location store.
    /// `layers` controls which feature categories to extract.
    ///
    /// This method streams blobs directly from disk via
    /// [`osmpbf::ElementReader::from_path`] and runs the decode pipeline
    /// in parallel via rayon. Memory usage is bounded by the decoded
    /// feature set and the way-geometry cache used for multipolygon
    /// assembly — NOT by the PBF file size. Works at any scale from a
    /// small city extract to the full planet file without branching.
    pub fn process(
        &self,
        pbf_path: &Path,
        node_store: &dyn NodeLocationStore,
        layers: &LayerSet,
    ) -> OsmicResult<ProcessedData> {
        let total_start = Instant::now();

        // Pass 1: Node locations. Streams blobs directly from disk.
        info!("Pass 1: Reading node locations from {}", pbf_path.display());
        let pass1_start = Instant::now();
        let node_count = self.pass1_nodes(pbf_path, node_store)?;
        let pass1_duration = pass1_start.elapsed();
        info!(
            "Pass 1 complete: {} nodes in {:.2}s ({:.0} nodes/s)",
            node_count,
            pass1_duration.as_secs_f64(),
            node_count as f64 / pass1_duration.as_secs_f64().max(1e-9)
        );

        // Pass 2: Ways + POI nodes + Relations in ONE decompression pass.
        // Way coords are cached in RAM for multipolygon assembly.
        info!("Pass 2: Processing ways, POI nodes, and relations (single streaming pass)...");
        let pass2_start = Instant::now();
        let (mut features, way_count, relation_count, bbox) =
            self.pass2_all(pbf_path, node_store, layers)?;
        let pass2_duration = pass2_start.elapsed();
        info!(
            "Pass 2 complete: {} ways + {} relations → {} features in {:.2}s",
            way_count,
            relation_count,
            features.len(),
            pass2_duration.as_secs_f64()
        );
        features.shrink_to_fit();

        let total_duration = total_start.elapsed();
        let feature_count = features.len() as u64;

        Ok(ProcessedData {
            tag_store: Arc::clone(&self.tag_store),
            features,
            bbox,
            stats: PipelineStats {
                node_count,
                way_count,
                relation_count,
                feature_count,
                pass1_duration,
                pass2_duration,
                total_duration,
            },
        })
    }

    /// Pass 1: Stream nodes from disk and store their locations.
    ///
    /// Uses [`ElementReader::from_path`] which internally streams blobs
    /// via [`osmpbf::BlobReader`] — no in-memory PBF buffer required.
    fn pass1_nodes(&self, pbf_path: &Path, node_store: &dyn NodeLocationStore) -> OsmicResult<u64> {
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

    /// Pass 2: Stream ways + POI nodes + relations in a single disk pass.
    ///
    /// Way coordinates are cached in memory for multipolygon assembly
    /// (the cache is bounded by the number of ways referenced by relations,
    /// not by the PBF size).
    fn pass2_all(
        &self,
        pbf_path: &Path,
        node_store: &dyn NodeLocationStore,
        layers: &LayerSet,
    ) -> OsmicResult<(Vec<Feature>, u64, u64, BBox)> {
        let reader =
            ElementReader::from_path(pbf_path).map_err(|e| OsmicError::Pbf(e.to_string()))?;
        let tag_store = &self.tag_store;

        // Single parallel pass: features + cached way coords + relation info
        let (features, way_geoms, relations, way_count, bbox, unresolved) = reader
            .par_map_reduce(
                |element| {
                    let mut local_features: Vec<Feature> = Vec::new();
                    let mut local_way_geoms: Vec<(i64, Vec<Coord<f64>>)> = Vec::new();
                    let mut local_relations: Vec<RelationInfo> = Vec::new();
                    let mut local_bbox = BBox::empty();
                    let mut local_way_count = 0u64;
                    let mut local_unresolved = 0u64;

                    match element {
                        Element::Way(way) => {
                            local_way_count = 1;

                            let mut tags = Tags::new();
                            for (k, v) in way.tags() {
                                tags.push(tag_store.intern_key(k), tag_store.intern_value(v));
                            }

                            // Resolve way coordinates
                            let refs: Vec<i64> = way.refs().collect();
                            let coords: Vec<Coord<f64>> = refs
                                .iter()
                                .filter_map(|&id| {
                                    node_store.get(id).map(|ll| Coord {
                                        x: ll.lon,
                                        y: ll.lat,
                                    })
                                })
                                .collect();

                            // Cache ALL way geometries for relation assembly
                            if coords.len() >= 2 {
                                local_way_geoms.push((way.id(), coords.clone()));
                            }

                            if let Some(kind) = classify::classify(&tags, tag_store, layers) {
                                if coords.len() >= 2 {
                                    for c in &coords {
                                        local_bbox.expand(c.x, c.y);
                                    }
                                    let is_closed = refs.len() >= 4 && refs.first() == refs.last();
                                    let mut geometry = if is_closed && kind.is_area() {
                                        Geometry::Polygon(Polygon::new(
                                            LineString::new(coords),
                                            vec![],
                                        ))
                                    } else {
                                        Geometry::Line(LineString::new(coords))
                                    };
                                    osmic_geo::orient::orient_geometry(&mut geometry);

                                    local_features.push(Feature {
                                        id: way.id(),
                                        kind,
                                        geometry,
                                        tags,
                                    });
                                } else if !refs.is_empty() {
                                    local_unresolved += 1;
                                }
                            }
                        }
                        Element::Node(node) => {
                            process_poi_node(
                                node.id(),
                                node.lon(),
                                node.lat(),
                                node.tags(),
                                tag_store,
                                layers,
                                &mut local_features,
                                &mut local_bbox,
                            );
                        }
                        Element::DenseNode(node) => {
                            process_poi_node(
                                node.id(),
                                node.lon(),
                                node.lat(),
                                node.tags(),
                                tag_store,
                                layers,
                                &mut local_features,
                                &mut local_bbox,
                            );
                        }
                        Element::Relation(rel) => {
                            let mut is_mp = false;
                            let mut tags = Tags::new();
                            for (k, v) in rel.tags() {
                                if k == "type" && v == "multipolygon" {
                                    is_mp = true;
                                }
                                tags.push(tag_store.intern_key(k), tag_store.intern_value(v));
                            }
                            if is_mp {
                                let mut outers = Vec::new();
                                let mut inners = Vec::new();
                                for member in rel.members() {
                                    if member.member_type == RelMemberType::Way {
                                        match member.role().unwrap_or("") {
                                            "outer" | "" => outers.push(member.member_id),
                                            "inner" => inners.push(member.member_id),
                                            _ => {}
                                        }
                                    }
                                }
                                if !outers.is_empty() {
                                    local_relations.push(RelationInfo {
                                        id: rel.id(),
                                        tags,
                                        outer_way_ids: outers,
                                        inner_way_ids: inners,
                                    });
                                }
                            }
                        }
                    }

                    (
                        local_features,
                        local_way_geoms,
                        local_relations,
                        local_way_count,
                        local_bbox,
                        local_unresolved,
                    )
                },
                || {
                    (
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        0u64,
                        BBox::empty(),
                        0u64,
                    )
                },
                |(mut fa, mut ga, mut ra, ca, mut ba, ua), (fb, gb, rb, cb, bb, ub)| {
                    fa.extend(fb);
                    ga.extend(gb);
                    ra.extend(rb);
                    ba.extend(&bb);
                    (fa, ga, ra, ca + cb, ba, ua + ub)
                },
            )
            .map_err(|e| OsmicError::Pbf(e.to_string()))?;

        if unresolved > 0 {
            info!(
                unresolved,
                "Ways skipped (nodes outside extract or exceeding max_node_id)"
            );
        }

        let mut features = features;
        let mut bbox = bbox;
        let relation_count = relations.len() as u64;

        // Assemble multipolygon relations from cached way geometries
        if !relations.is_empty() {
            info!(
                relations = relation_count,
                cached_ways = way_geoms.len(),
                "Assembling multipolygon relations from cached way coords"
            );

            // Build lookup from cached way coords
            let needed_ways: HashSet<i64> = relations
                .iter()
                .flat_map(|r| r.outer_way_ids.iter().chain(r.inner_way_ids.iter()))
                .copied()
                .collect();

            let way_geom_map: HashMap<i64, Vec<Coord<f64>>> = way_geoms
                .into_iter()
                .filter(|(id, _)| needed_ways.contains(id))
                .collect();

            info!(
                resolved = way_geom_map.len(),
                needed = needed_ways.len(),
                "Member ways resolved from cache"
            );

            let tag_store = &self.tag_store;
            for rel in &relations {
                let kind = match classify::classify(&rel.tags, tag_store, layers) {
                    Some(k) => k,
                    None => continue,
                };
                match crate::multipolygon::assemble_multipolygon(
                    &rel.outer_way_ids,
                    &rel.inner_way_ids,
                    &way_geom_map,
                ) {
                    Ok((geometry, report)) => {
                        if !report.warnings.is_empty() {
                            tracing::debug!(
                                rel_id = rel.id,
                                warnings = report.warnings.len(),
                                "multipolygon assembled with warnings"
                            );
                        }
                        let bb = geometry.bbox();
                        bbox.expand(bb.min_lon, bb.min_lat);
                        bbox.expand(bb.max_lon, bb.max_lat);
                        features.push(Feature {
                            id: rel.id,
                            kind,
                            geometry,
                            tags: rel.tags.clone(),
                        });
                    }
                    Err(e) => {
                        tracing::debug!(rel_id = rel.id, error = ?e, "multipolygon skipped");
                    }
                }
            }

            info!(features = features.len(), "Multipolygon features assembled");
        }

        Ok((features, way_count, relation_count, bbox))
    }
}

/// Extract a tagged node as a POI point feature if it classifies.
// All arguments are distinct positional inputs; a config struct would obscure the data flow.
#[allow(clippy::too_many_arguments)]
fn process_poi_node<'a>(
    id: i64,
    lon: f64,
    lat: f64,
    tags_iter: impl Iterator<Item = (&'a str, &'a str)>,
    tag_store: &TagStore,
    layers: &LayerSet,
    features: &mut Vec<Feature>,
    bbox: &mut BBox,
) {
    let mut tags = Tags::new();
    let mut has_tags = false;
    for (k, v) in tags_iter {
        has_tags = true;
        tags.push(tag_store.intern_key(k), tag_store.intern_value(v));
    }
    if !has_tags {
        return;
    }
    if let Some(kind) = classify::classify(&tags, tag_store, layers) {
        bbox.expand(lon, lat);
        features.push(Feature {
            id,
            kind,
            geometry: Geometry::Point(geo_types::Point::new(lon, lat)),
            tags,
        });
    }
}

struct RelationInfo {
    id: i64,
    tags: Tags,
    outer_way_ids: Vec<i64>,
    inner_way_ids: Vec<i64>,
}

impl Default for PbfProcessor {
    fn default() -> Self {
        Self::new()
    }
}
