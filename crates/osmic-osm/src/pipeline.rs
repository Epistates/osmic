//! Two-pass PBF → feature pipeline (requires the `native` feature).
//!
//! **Pass 1** decodes every block in parallel and
//! - stores node locations in the configured [`NodeStorage`] (skipped for
//!   files with locations on ways), and
//! - collects `type=multipolygon` / `type=boundary` relations that classify
//!   into an enabled layer, together with their member way ids.
//!
//! **Pass 2** decodes every block again and
//! - turns tagged nodes into point features,
//! - resolves way geometry only for ways that classify or that a relation
//!   needs (all other ways are skipped without touching the node index),
//! - caches coordinates of relation member ways only,
//! - streams features to a [`FeatureSink`] block by block.
//!
//! Finally relations are assembled into areas in parallel.
//!
//! Ways or relations whose members are missing from the input (typical at
//! the edge of an extract) are counted in [`PipelineStats`] and skipped by
//! default rather than emitted with distorted geometry.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use geo_types::{Coord, LineString, Point, Polygon};
use osmpbf::{Element, PrimitiveBlock, RelMemberType};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{info, warn};

use osmic_core::{BBox, FixedCoord, Geometry, NodeLocationStore, OsmId, OsmType};
use osmic_index::{DenseNodeStore, NodeIndex, NodeRun, SparseNodeIndex};

use crate::classify::{Classified, KeyValues, classify, closed_way_is_area};
use crate::error::OsmError;
use crate::feature::Feature;
use crate::filter::TagFilter;
use crate::layers::LayerSet;
use crate::multipolygon::{MemberWay, Role, assemble_area};
use crate::pbf::{PbfHeader, StringTable, par_blocks, read_header};
use crate::tags::{TagRetention, TagStore};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Where node locations are kept while processing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NodeStorage {
    /// Sparse in-memory index: ~8 bytes per node present in the input.
    #[default]
    Sparse,
    /// Dense in-memory array indexed by node id (8 bytes per possible id;
    /// only pages that receive nodes use RAM).
    DenseMemory { max_node_id: i64 },
    /// Dense persistent file, reusable for replication updates.
    DenseFile { path: PathBuf, max_node_id: i64 },
}

/// What to do with a way some of whose nodes are missing from the input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IncompleteWays {
    /// Skip the way (and any relation using it). Never emits wrong
    /// geometry.
    #[default]
    Skip,
    /// Build the way from the nodes that are present, if at least two are.
    /// A closed way missing its first/last node is no longer treated as
    /// closed.
    KeepAvailable,
}

/// Pipeline configuration.
#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    /// Layers to classify into; elements matching no enabled layer are
    /// skipped.
    pub layers: LayerSet,
    pub node_storage: NodeStorage,
    /// Which tags to keep on emitted features.
    pub tag_retention: TagRetention,
    pub incomplete_ways: IncompleteWays,
    /// Only elements whose (raw) tags match become features. Applied
    /// before classification, so it can test any key — including ones
    /// `tag_retention` drops.
    pub filter: Option<TagFilter>,
}

/// Receives features as the pipeline produces them, in batches, from many
/// threads.
pub trait FeatureSink: Sync {
    fn accept(&self, features: Vec<Feature>) -> Result<(), BoxError>;
}

/// A sink that collects every feature in memory.
#[derive(Debug, Default)]
pub struct CollectSink(Mutex<Vec<Feature>>);

impl CollectSink {
    /// All collected features, sorted by element id then layer so the
    /// result does not depend on thread scheduling.
    pub fn into_sorted(self) -> Vec<Feature> {
        let mut v = self.0.into_inner().unwrap_or_else(|p| p.into_inner());
        v.sort_by_key(|f| (f.id, f.kind.layer()));
        v
    }
}

impl FeatureSink for CollectSink {
    fn accept(&self, mut features: Vec<Feature>) -> Result<(), BoxError> {
        self.0
            .lock()
            .map_err(|_| "collect sink poisoned")?
            .append(&mut features);
        Ok(())
    }
}

/// Statistics from one pipeline run.
#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    pub node_count: u64,
    pub way_count: u64,
    pub relation_count: u64,
    pub feature_count: u64,
    /// Nodes with coordinates outside WGS84 bounds (ignored).
    pub invalid_nodes: u64,
    /// Ways with at least one node missing from the input.
    pub incomplete_ways: u64,
    /// Multipolygon/boundary relations that classify into an enabled layer.
    pub area_relations: u64,
    pub assembled_relations: u64,
    /// Relations with member ways missing from the input.
    pub incomplete_relations: u64,
    /// Relations whose rings could not be closed or enclosed no area.
    pub invalid_relations: u64,
    /// Member roles that disagreed with the assembled geometry.
    pub role_mismatches: u64,
    /// Memory used by the node index.
    pub node_index_bytes: u64,
    pub pass1_duration: Duration,
    pub pass2_duration: Duration,
    pub total_duration: Duration,
}

/// Result of a streaming run.
pub struct RunOutput {
    pub header: PbfHeader,
    /// Bounding box of every emitted feature.
    pub bbox: BBox,
    pub stats: PipelineStats,
    /// The node index built in pass 1 (`None` for files with locations on
    /// ways).
    pub node_index: Option<NodeIndex>,
}

/// Result of [`PbfProcessor::process`].
pub struct ProcessedData {
    pub header: PbfHeader,
    pub tag_store: Arc<TagStore>,
    /// Features sorted by element id, then layer.
    pub features: Vec<Feature>,
    pub bbox: BBox,
    pub stats: PipelineStats,
}

/// An area relation collected in pass 1.
struct AreaRelation {
    id: i64,
    tags: Vec<(String, String)>,
    members: Vec<(i64, Role)>,
}

struct Pass1Block {
    run: Option<osmic_index::SealedNodeRun>,
    relations: Vec<RelationRecord>,
    nodes: u64,
    ways: u64,
    relation_count: u64,
    invalid_nodes: u64,
}

#[derive(Default)]
struct Pass2Block {
    cached: Vec<(i64, Vec<FixedCoord>)>,
    features: u64,
    incomplete_ways: u64,
    bbox: BBox,
}

/// PBF → feature processor.
pub struct PbfProcessor {
    config: PipelineConfig,
    tag_store: Arc<TagStore>,
}

impl PbfProcessor {
    pub fn new(config: PipelineConfig) -> Self {
        Self::with_tag_store(config, Arc::new(TagStore::new()))
    }

    /// Use an existing tag store (e.g. one shared with another dataset).
    pub fn with_tag_store(config: PipelineConfig, tag_store: Arc<TagStore>) -> Self {
        Self { config, tag_store }
    }

    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// The interner holding every tag of every emitted feature.
    pub fn tag_store(&self) -> &Arc<TagStore> {
        &self.tag_store
    }

    /// Process `path` and collect all features in memory.
    pub fn process(&self, path: &Path) -> Result<ProcessedData, OsmError> {
        let sink = CollectSink::default();
        let out = self.run(path, &sink)?;
        Ok(ProcessedData {
            header: out.header,
            tag_store: Arc::clone(&self.tag_store),
            features: sink.into_sorted(),
            bbox: out.bbox,
            stats: out.stats,
        })
    }

    /// Process `path`, streaming features into `sink`.
    pub fn run(&self, path: &Path, sink: &dyn FeatureSink) -> Result<RunOutput, OsmError> {
        let total_start = Instant::now();
        let mut stats = PipelineStats::default();

        // ── Pass 1 ──────────────────────────────────────────────────────
        let layers = self.config.layers;
        let filter = self.config.filter.as_ref();
        let select = move |tags: &[(&str, &str)]| {
            let kv = KeyValues::scan(tags.iter().copied());
            matches!(kv.relation_type(), Some("multipolygon" | "boundary"))
                && !classify(&kv, layers).is_empty()
                && filter.is_none_or(|f| f.matches(tags))
        };
        let scan = scan_nodes(path, &self.config.node_storage, &select)?;
        stats.node_count = scan.node_count;
        stats.way_count = scan.way_count;
        stats.relation_count = scan.relation_count;
        stats.invalid_nodes = scan.invalid_nodes;
        stats.node_index_bytes = scan.node_index_bytes;
        stats.pass1_duration = scan.duration;
        let header = scan.header;
        let node_index = scan.index;
        let relations: Vec<AreaRelation> = scan
            .relations
            .into_iter()
            .filter_map(AreaRelation::from_record)
            .collect();
        let needed_ways: FxHashSet<i64> = relations
            .iter()
            .flat_map(|r| r.members.iter().map(|&(id, _)| id))
            .collect();
        stats.area_relations = relations.len() as u64;

        // ── Pass 2 ──────────────────────────────────────────────────────
        let pass2_start = Instant::now();
        info!("Pass 2: features");
        let ctx = Pass2Context {
            index: node_index.as_ref(),
            needed_ways: &needed_ways,
            config: &self.config,
            tag_store: &self.tag_store,
            sink,
        };
        let blocks = par_blocks(path, |block| ctx.block(block))?;
        let mut bbox = BBox::empty();
        let mut way_cache: FxHashMap<i64, Vec<FixedCoord>> = FxHashMap::default();
        for (_, b) in blocks {
            stats.feature_count += b.features;
            stats.incomplete_ways += b.incomplete_ways;
            bbox.extend(&b.bbox);
            way_cache.extend(b.cached);
        }

        // ── Relations ───────────────────────────────────────────────────
        let outcomes: Vec<RelationOutcome> = relations
            .par_chunks(256)
            .map(|chunk| ctx.relations(chunk, &way_cache))
            .collect::<Result<_, _>>()?;
        for o in outcomes {
            stats.feature_count += o.features;
            stats.assembled_relations += o.assembled;
            stats.incomplete_relations += o.incomplete;
            stats.invalid_relations += o.invalid;
            stats.role_mismatches += o.role_mismatches;
            bbox.extend(&o.bbox);
        }
        drop(way_cache);
        stats.pass2_duration = pass2_start.elapsed();
        stats.total_duration = total_start.elapsed();

        info!(
            features = stats.feature_count,
            assembled_relations = stats.assembled_relations,
            secs = stats.pass2_duration.as_secs_f64(),
            "Pass 2 complete"
        );
        if stats.incomplete_ways > 0 || stats.incomplete_relations > 0 {
            warn!(
                incomplete_ways = stats.incomplete_ways,
                incomplete_relations = stats.incomplete_relations,
                policy = ?self.config.incomplete_ways,
                "Input references objects it does not contain (normal for extracts); \
                 affected features were skipped"
            );
        }
        if stats.invalid_relations > 0 || stats.invalid_nodes > 0 {
            warn!(
                invalid_relations = stats.invalid_relations,
                invalid_nodes = stats.invalid_nodes,
                "Invalid data skipped"
            );
        }

        Ok(RunOutput {
            header,
            bbox,
            stats,
            node_index,
        })
    }
}

/// Decides, from its tags, whether [`scan_nodes`] collects a relation.
pub type RelationSelector<'a> = dyn Fn(&[(&str, &str)]) -> bool + Sync + 'a;

/// A member of a relation selected by [`scan_nodes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationMember {
    pub osm_type: OsmType,
    pub id: i64,
    pub role: String,
}

/// A relation selected by [`scan_nodes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationRecord {
    pub id: i64,
    pub tags: Vec<(String, String)>,
    pub members: Vec<RelationMember>,
}

/// Result of [`scan_nodes`].
pub struct NodeScan {
    pub header: PbfHeader,
    /// Node locations (`None` for files with locations on ways).
    pub index: Option<NodeIndex>,
    /// Relations accepted by the selector, in file order.
    pub relations: Vec<RelationRecord>,
    pub node_count: u64,
    pub way_count: u64,
    pub relation_count: u64,
    /// Nodes with coordinates outside WGS84 bounds (not stored).
    pub invalid_nodes: u64,
    pub node_index_bytes: u64,
    pub duration: Duration,
}

/// Pass 1 over a PBF file: store every node location in `storage` and
/// collect the relations for which `select(tags)` returns true.
///
/// Relations are collected here because they come last in sorted files:
/// knowing them up front lets pass 2 cache exactly the member ways it
/// needs.
pub fn scan_nodes(
    path: &Path,
    storage: &NodeStorage,
    select: &RelationSelector<'_>,
) -> Result<NodeScan, OsmError> {
    let start = Instant::now();
    let header = read_header(path)?;
    let locations_on_ways = header.has_locations_on_ways();
    info!(path = %path.display(), "Pass 1: node locations and relations");
    let dense = match storage {
        _ if locations_on_ways => None,
        NodeStorage::Sparse => None,
        NodeStorage::DenseMemory { max_node_id } => Some(DenseNodeStore::in_memory(*max_node_id)?),
        NodeStorage::DenseFile { path, max_node_id } => {
            Some(DenseNodeStore::create(path, *max_node_id)?)
        }
    };
    let collect_sparse = !locations_on_ways && dense.is_none();
    let blocks = par_blocks(path, |block| {
        scan_block(block, select, collect_sparse, dense.as_ref())
    })?;

    let mut scan = NodeScan {
        header,
        index: None,
        relations: Vec::new(),
        node_count: 0,
        way_count: 0,
        relation_count: 0,
        invalid_nodes: 0,
        node_index_bytes: 0,
        duration: Duration::ZERO,
    };
    let mut runs = Vec::new();
    for (_, b) in blocks {
        scan.node_count += b.nodes;
        scan.way_count += b.ways;
        scan.relation_count += b.relation_count;
        scan.invalid_nodes += b.invalid_nodes;
        scan.relations.extend(b.relations);
        runs.extend(b.run);
    }
    scan.index = if locations_on_ways {
        None
    } else if let Some(d) = dense {
        d.flush()?;
        scan.node_index_bytes = (d.capacity() * 8) as u64;
        Some(NodeIndex::Dense(d))
    } else {
        let sparse = SparseNodeIndex::from_runs(runs);
        scan.node_index_bytes = sparse.heap_bytes() as u64;
        Some(NodeIndex::Sparse(sparse))
    };
    scan.duration = start.elapsed();
    info!(
        nodes = scan.node_count,
        ways = scan.way_count,
        relations = scan.relation_count,
        selected_relations = scan.relations.len(),
        node_index_mib = scan.node_index_bytes >> 20,
        secs = scan.duration.as_secs_f64(),
        "Pass 1 complete"
    );
    Ok(scan)
}

fn scan_block(
    block: &PrimitiveBlock,
    select: &RelationSelector<'_>,
    collect_sparse: bool,
    dense: Option<&DenseNodeStore>,
) -> Result<Pass1Block, OsmError> {
    let mut out = Pass1Block {
        run: None,
        relations: Vec::new(),
        nodes: 0,
        ways: 0,
        relation_count: 0,
        invalid_nodes: 0,
    };
    let mut run = NodeRun::default();
    let mut store = |id: i64, c: FixedCoord, out: &mut Pass1Block| -> Result<(), OsmError> {
        out.nodes += 1;
        if !c.is_valid() {
            out.invalid_nodes += 1;
            return Ok(());
        }
        if let Some(d) = dense {
            d.set(id, c)?;
        } else if collect_sparse {
            run.push(id, c);
        }
        Ok(())
    };
    let mut tags: Vec<(&str, &str)> = Vec::new();
    for element in block.elements() {
        match element {
            Element::DenseNode(n) => {
                store(
                    n.id(),
                    FixedCoord::new(n.decimicro_lon(), n.decimicro_lat()),
                    &mut out,
                )?;
            }
            Element::Node(n) => {
                store(
                    n.id(),
                    FixedCoord::new(n.decimicro_lon(), n.decimicro_lat()),
                    &mut out,
                )?;
            }
            Element::Way(_) => out.ways += 1,
            Element::Relation(rel) => {
                out.relation_count += 1;
                tags.clear();
                tags.extend(rel.tags());
                if !select(&tags) {
                    continue;
                }
                out.relations.push(RelationRecord {
                    id: rel.id(),
                    tags: tags
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                    members: rel
                        .members()
                        .map(|m| RelationMember {
                            osm_type: match m.member_type {
                                RelMemberType::Node => OsmType::Node,
                                RelMemberType::Way => OsmType::Way,
                                RelMemberType::Relation => OsmType::Relation,
                            },
                            id: m.member_id,
                            role: m.role().unwrap_or_default().to_owned(),
                        })
                        .collect(),
                });
            }
        }
    }
    if collect_sparse && !run.is_empty() {
        out.run = Some(run.seal());
    }
    Ok(out)
}

impl AreaRelation {
    /// Way members with area roles (each way once — a way listed twice
    /// would cancel its own segments during assembly) and tags without
    /// `type`. `None` if no member way remains.
    fn from_record(r: RelationRecord) -> Option<Self> {
        let mut seen = FxHashSet::default();
        let members: Vec<(i64, Role)> = r
            .members
            .iter()
            .filter(|m| m.osm_type == OsmType::Way)
            .filter_map(|m| Some((m.id, Role::parse(&m.role)?)))
            .filter(|(id, _)| seen.insert(*id))
            .collect();
        if members.is_empty() {
            return None;
        }
        Some(Self {
            id: r.id,
            tags: r.tags.into_iter().filter(|(k, _)| k != "type").collect(),
            members,
        })
    }
}

struct Pass2Context<'a> {
    index: Option<&'a NodeIndex>,
    needed_ways: &'a FxHashSet<i64>,
    config: &'a PipelineConfig,
    tag_store: &'a TagStore,
    sink: &'a dyn FeatureSink,
}

#[derive(Default)]
struct RelationOutcome {
    features: u64,
    assembled: u64,
    incomplete: u64,
    invalid: u64,
    role_mismatches: u64,
    bbox: BBox,
}

impl Pass2Context<'_> {
    fn emit(&self, features: Vec<Feature>) -> Result<(), OsmError> {
        if features.is_empty() {
            return Ok(());
        }
        self.sink.accept(features).map_err(OsmError::Sink)
    }

    fn make_features<'t>(
        &self,
        id: OsmId,
        classes: &[Classified<'_>],
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        geometry_for: impl Fn(&Classified<'_>) -> Option<Geometry>,
        out: &mut Vec<Feature>,
        bbox: &mut BBox,
    ) {
        let interned = self.tag_store.intern_tags(tags, &self.config.tag_retention);
        for c in classes {
            if let Some(geometry) = geometry_for(c) {
                bbox.extend(&geometry.bbox());
                out.push(Feature {
                    id,
                    kind: c.kind,
                    geometry,
                    tags: interned.clone(),
                });
            }
        }
    }

    fn block(&self, block: &PrimitiveBlock) -> Result<Pass2Block, OsmError> {
        let mut out = Pass2Block {
            bbox: BBox::empty(),
            ..Default::default()
        };
        let mut features: Vec<Feature> = Vec::new();
        let layers = self.config.layers;
        let mut coords: Vec<FixedCoord> = Vec::new();
        let mut refs: Vec<i64> = Vec::new();
        let mut tag_buf: Vec<(&str, &str)> = Vec::new();
        let filter = self.config.filter.as_ref();
        // Whether an element's tags pass the configured filter.
        macro_rules! passes {
            ($tags:expr) => {
                match filter {
                    None => true,
                    Some(f) => {
                        tag_buf.clear();
                        tag_buf.extend($tags);
                        f.matches(&tag_buf)
                    }
                }
            };
        }

        let strings = StringTable::new(block);
        for element in block.elements() {
            match element {
                Element::DenseNode(n) => {
                    let tags = strings.tags(n.raw_tags());
                    let kv = KeyValues::scan(tags.clone());
                    if kv.is_empty() {
                        continue;
                    }
                    let c = FixedCoord::new(n.decimicro_lon(), n.decimicro_lat());
                    let classes = classify(&kv, layers);
                    if classes.is_empty() || !c.is_valid() || !passes!(tags.clone()) {
                        continue;
                    }
                    let point = Geometry::Point(Point(c.to_coord()));
                    self.make_features(
                        OsmId::node(n.id()),
                        &classes,
                        tags,
                        |_| Some(point.clone()),
                        &mut features,
                        &mut out.bbox,
                    );
                }
                Element::Node(n) => {
                    let tags = strings.tags(n.raw_tags());
                    let kv = KeyValues::scan(tags.clone());
                    if kv.is_empty() {
                        continue;
                    }
                    let c = FixedCoord::new(n.decimicro_lon(), n.decimicro_lat());
                    let classes = classify(&kv, layers);
                    if classes.is_empty() || !c.is_valid() || !passes!(tags.clone()) {
                        continue;
                    }
                    let point = Geometry::Point(Point(c.to_coord()));
                    self.make_features(
                        OsmId::node(n.id()),
                        &classes,
                        tags,
                        |_| Some(point.clone()),
                        &mut features,
                        &mut out.bbox,
                    );
                }
                Element::Way(way) => {
                    let needed = self.needed_ways.contains(&way.id());
                    let tags = strings.tags(way.raw_tags());
                    let kv = KeyValues::scan(tags.clone());
                    let mut classes = if kv.is_empty() {
                        Default::default()
                    } else {
                        classify(&kv, layers)
                    };
                    if !classes.is_empty() && !passes!(tags.clone()) {
                        // Filtered out as a feature, but a relation may
                        // still need its geometry.
                        classes.clear();
                    }
                    if classes.is_empty() && !needed {
                        continue;
                    }

                    // Resolve coordinates.
                    refs.clear();
                    refs.extend(way.refs());
                    coords.clear();
                    let mut missing = false;
                    match self.index {
                        Some(index) => {
                            for &r in &refs {
                                match index.get(r) {
                                    Some(c) => coords.push(c),
                                    None => missing = true,
                                }
                            }
                        }
                        None => coords.extend(
                            way.node_locations()
                                .map(|l| FixedCoord::new(l.decimicro_lon(), l.decimicro_lat())),
                        ),
                    }
                    let closed = refs.len() >= 4 && refs.first() == refs.last();

                    if missing {
                        out.incomplete_ways += 1;
                        if self.config.incomplete_ways == IncompleteWays::Skip {
                            continue;
                        }
                    } else if needed {
                        out.cached.push((way.id(), coords.clone()));
                    }
                    if classes.is_empty() || coords.len() < 2 {
                        continue;
                    }

                    let closed = closed && !missing && coords.len() >= 4;
                    let line: Vec<Coord<f64>> = coords.iter().map(|c| c.to_coord()).collect();
                    let area = kv.area();
                    self.make_features(
                        OsmId::way(way.id()),
                        &classes,
                        tags,
                        |c| {
                            let ls = LineString(line.clone());
                            Some(if closed && closed_way_is_area(c, area) {
                                let mut g = Geometry::Polygon(Polygon::new(ls, vec![]));
                                osmic_geo::orient_geometry(&mut g);
                                g
                            } else {
                                Geometry::Line(ls)
                            })
                        },
                        &mut features,
                        &mut out.bbox,
                    );
                }
                Element::Relation(_) => {}
            }
        }
        out.features = features.len() as u64;
        self.emit(features)?;
        Ok(out)
    }

    fn relations(
        &self,
        chunk: &[AreaRelation],
        way_cache: &FxHashMap<i64, Vec<FixedCoord>>,
    ) -> Result<RelationOutcome, OsmError> {
        let mut out = RelationOutcome {
            bbox: BBox::empty(),
            ..Default::default()
        };
        let mut features = Vec::new();
        for rel in chunk {
            let mut members = Vec::with_capacity(rel.members.len());
            let mut incomplete = false;
            for &(way_id, role) in &rel.members {
                match way_cache.get(&way_id) {
                    Some(coords) => members.push(MemberWay {
                        id: way_id,
                        role,
                        coords,
                    }),
                    None => incomplete = true,
                }
            }
            if incomplete {
                out.incomplete += 1;
                continue;
            }
            let (geometry, report) = match assemble_area(&members) {
                Ok(ok) => ok,
                Err(e) => {
                    out.invalid += 1;
                    tracing::debug!(relation = rel.id, error = %e, "relation not assembled");
                    continue;
                }
            };
            out.assembled += 1;
            out.role_mismatches += report.role_mismatches as u64;
            let kv = KeyValues::scan(rel.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            let classes = classify(&kv, self.config.layers);
            self.make_features(
                OsmId::relation(rel.id),
                &classes,
                rel.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                |_| Some(geometry.clone()),
                &mut features,
                &mut out.bbox,
            );
        }
        out.features = features.len() as u64;
        self.emit(features)?;
        Ok(out)
    }
}

impl Default for PbfProcessor {
    fn default() -> Self {
        Self::new(PipelineConfig::default())
    }
}
