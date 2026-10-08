//! End-to-end tests of the PBF pipeline on generated fixtures.

use std::path::{Path, PathBuf};

use geo::Winding;
use osmic_core::{FixedCoord, Geometry, OsmId, OsmType};
use osmic_osm::feature::{AmenityKind, BuildingKind, FeatureKind, HighwayKind, LanduseKind};
use osmic_osm::pbf::{PbfWriter, PbfWriterOptions};
use osmic_osm::{
    Feature, IncompleteWays, LayerSet, NodeStorage, OsmError, PbfProcessor, PipelineConfig,
    ProcessedData, TagRetention,
};

/// (id, location, tags)
type FixtureNode<'a> = (i64, FixedCoord, Vec<(&'a str, &'a str)>);

fn fc(lon: i32, lat: i32) -> FixedCoord {
    FixedCoord::new(lon, lat)
}

/// Writes a small but representative dataset:
///
/// - node 1: a tagged café POI with 7-decimal coordinates
/// - way 10: a road
/// - way 11: a closed school with a building tag (two layers, both areas)
/// - way 12: a closed roundabout road (closed but not an area)
/// - way 13: a road referencing missing node 999 (incomplete)
/// - relation 100: landuse multipolygon, outer way 20 + inner way 21
/// - relation 101: multipolygon whose outer way 22 is missing (incomplete)
/// - relation 102: route relation (ignored)
fn write_fixture(path: &Path, sorted: bool, shuffle_nodes: bool) {
    let opts = PbfWriterOptions::new().sorted(sorted);
    let mut w =
        PbfWriter::new(std::fs::File::create(path).expect("create"), &opts).expect("header");

    let mut nodes: Vec<FixtureNode<'_>> = vec![
        (
            1,
            fc(-1_224_194_155, 377_749_295),
            vec![("amenity", "cafe"), ("name", "Café Ünïcode")],
        ),
        // Road 10.
        (2, fc(0, 0), vec![]),
        (3, fc(1_000, 0), vec![]),
        // School 11.
        (4, fc(10_000, 10_000), vec![]),
        (5, fc(10_100, 10_000), vec![]),
        (6, fc(10_100, 10_100), vec![]),
        (7, fc(10_000, 10_100), vec![]),
        // Roundabout 12.
        (8, fc(20_000, 20_000), vec![]),
        (9, fc(20_100, 20_000), vec![]),
        (15, fc(20_100, 20_100), vec![]),
        // Landuse outer 20 (square) and inner 21 (smaller square).
        (30, fc(0, 50_000), vec![]),
        (31, fc(1_000, 50_000), vec![]),
        (32, fc(1_000, 51_000), vec![]),
        (33, fc(0, 51_000), vec![]),
        (34, fc(200, 50_200), vec![]),
        (35, fc(400, 50_200), vec![]),
        (36, fc(400, 50_400), vec![]),
        (37, fc(200, 50_400), vec![]),
    ];
    if shuffle_nodes {
        nodes.reverse();
    }
    for (id, c, tags) in &nodes {
        w.write_node(*id, *c, tags).expect("node");
    }
    w.write_way(10, &[2, 3], &[("highway", "primary"), ("name", "Main St")])
        .expect("way");
    w.write_way(
        11,
        &[4, 5, 6, 7, 4],
        &[("amenity", "school"), ("building", "yes")],
    )
    .expect("way");
    w.write_way(
        12,
        &[8, 9, 15, 8],
        &[("highway", "residential"), ("junction", "roundabout")],
    )
    .expect("way");
    w.write_way(13, &[2, 999, 3], &[("highway", "service")])
        .expect("way");
    w.write_way(20, &[30, 31, 32, 33, 30], &[]).expect("way");
    w.write_way(21, &[34, 35, 36, 37, 34], &[]).expect("way");
    w.write_relation(
        100,
        &[(OsmType::Way, 20, "outer"), (OsmType::Way, 21, "inner")],
        &[("type", "multipolygon"), ("landuse", "grass")],
    )
    .expect("relation");
    w.write_relation(
        101,
        &[(OsmType::Way, 22, "outer")],
        &[("type", "multipolygon"), ("landuse", "forest")],
    )
    .expect("relation");
    w.write_relation(
        102,
        &[(OsmType::Way, 10, "")],
        &[("type", "route"), ("route", "bus")],
    )
    .expect("relation");
    w.finish().expect("finish");
}

fn fixture(name: &str, sorted: bool, shuffle: bool) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    write_fixture(&path, sorted, shuffle);
    (dir, path)
}

fn run(path: &Path, storage: NodeStorage) -> ProcessedData {
    PbfProcessor::new(PipelineConfig {
        node_storage: storage,
        ..Default::default()
    })
    .process(path)
    .expect("process")
}

fn find(data: &ProcessedData, id: OsmId, layer: &str) -> Feature {
    data.features
        .iter()
        .find(|f| f.id == id && f.kind.layer_name() == layer)
        .unwrap_or_else(|| panic!("no {layer} feature for {id}"))
        .clone()
}

#[test]
fn features_stats_and_geometry() {
    let (_dir, path) = fixture("a.osm.pbf", true, false);
    let data = run(&path, NodeStorage::Sparse);
    let s = &data.stats;
    assert_eq!((s.node_count, s.way_count, s.relation_count), (18, 6, 3));
    assert_eq!(s.incomplete_ways, 1, "way 13 references a missing node");
    assert_eq!(s.area_relations, 2, "route relation is not an area");
    assert_eq!(s.assembled_relations, 1);
    assert_eq!(
        s.incomplete_relations, 1,
        "relation 101's outer way is missing"
    );
    assert_eq!(s.feature_count, data.features.len() as u64);

    // Exact coordinates: no precision loss through the node index.
    let cafe = find(&data, OsmId::node(1), "amenity");
    assert_eq!(cafe.kind, FeatureKind::Amenity(AmenityKind::Cafe));
    let Geometry::Point(p) = cafe.geometry else {
        panic!("point")
    };
    assert_eq!(
        FixedCoord::from_degrees(p.x(), p.y()),
        Some(fc(-1_224_194_155, 377_749_295))
    );
    let name = data
        .tag_store
        .resolve_tags(&cafe.tags)
        .find(|(k, _)| *k == "name")
        .map(|(_, v)| v);
    assert_eq!(name, Some("Café Ünïcode"));

    // A school with a building tag produces both layers, both areas.
    let school = find(&data, OsmId::way(11), "amenity");
    let building = find(&data, OsmId::way(11), "building");
    assert_eq!(building.kind, FeatureKind::Building(BuildingKind::Yes));
    for f in [&school, &building] {
        let Geometry::Polygon(poly) = &f.geometry else {
            panic!("{:?} should be an area", f.kind)
        };
        assert!(poly.exterior().is_ccw());
    }

    // A closed road is a line, not an area.
    let roundabout = find(&data, OsmId::way(12), "highway");
    assert_eq!(
        roundabout.kind,
        FeatureKind::Highway(HighwayKind::Residential)
    );
    assert!(matches!(roundabout.geometry, Geometry::Line(_)));

    // The incomplete way is skipped, not emitted with a missing vertex.
    assert!(data.features.iter().all(|f| f.id != OsmId::way(13)));

    // Multipolygon with a hole; member ways themselves are untagged.
    let grass = find(&data, OsmId::relation(100), "landuse");
    assert_eq!(grass.kind, FeatureKind::Landuse(LanduseKind::Grass));
    let Geometry::Polygon(poly) = &grass.geometry else {
        panic!("polygon")
    };
    assert_eq!(poly.interiors().len(), 1);
    assert!(poly.interiors()[0].is_cw());
    assert!(data.features.iter().all(|f| f.id != OsmId::way(20)));
    assert!(data.features.iter().all(|f| f.id != OsmId::relation(101)));

    // Bbox covers every feature.
    assert!(data.bbox.contains_point(-122.4194155, 37.7749295));
}

#[test]
fn every_node_storage_gives_identical_output() {
    let (dir, path) = fixture("b.osm.pbf", true, false);
    let sparse = run(&path, NodeStorage::Sparse);
    let dense = run(&path, NodeStorage::DenseMemory { max_node_id: 1_000 });
    let file = run(
        &path,
        NodeStorage::DenseFile {
            path: dir.path().join("nodes.bin"),
            max_node_id: 1_000,
        },
    );
    let summary = |d: &ProcessedData| -> Vec<(OsmId, FeatureKind, String)> {
        d.features
            .iter()
            .map(|f| (f.id, f.kind, format!("{:?}", f.geometry)))
            .collect()
    };
    assert_eq!(summary(&sparse), summary(&dense));
    assert_eq!(summary(&sparse), summary(&file));
    assert!(
        dir.path().join("nodes.bin").exists(),
        "file store persists for replication"
    );
}

#[test]
fn dense_store_too_small_is_an_error_not_silent_loss() {
    let (_dir, path) = fixture("c.osm.pbf", true, false);
    let err = PbfProcessor::new(PipelineConfig {
        node_storage: NodeStorage::DenseMemory { max_node_id: 10 },
        ..Default::default()
    })
    .process(&path)
    .err()
    .expect("node ids above 10 must fail");
    assert!(matches!(err, OsmError::NodeStore(_)), "{err}");
}

#[test]
fn unsorted_input_produces_the_same_features() {
    let (_d1, sorted) = fixture("sorted.osm.pbf", true, false);
    let (_d2, unsorted) = fixture("unsorted.osm.pbf", false, true);
    let a = run(&sorted, NodeStorage::Sparse);
    let b = run(&unsorted, NodeStorage::Sparse);
    let ids = |d: &ProcessedData| {
        d.features
            .iter()
            .map(|f| (f.id, f.kind))
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&a), ids(&b));
}

#[test]
fn keep_available_policy_builds_partial_ways() {
    let (_dir, path) = fixture("d.osm.pbf", true, false);
    let data = PbfProcessor::new(PipelineConfig {
        incomplete_ways: IncompleteWays::KeepAvailable,
        ..Default::default()
    })
    .process(&path)
    .expect("process");
    let service = find(&data, OsmId::way(13), "highway");
    let Geometry::Line(l) = service.geometry else {
        panic!("line")
    };
    assert_eq!(l.0.len(), 2, "the two nodes that exist");
}

#[test]
fn layer_filter_and_tag_retention() {
    let (_dir, path) = fixture("e.osm.pbf", true, false);
    let data = PbfProcessor::new(PipelineConfig {
        layers: LayerSet::from_names("amenity").expect("valid"),
        tag_retention: TagRetention::Keys(vec!["name".to_string()].into()),
        ..Default::default()
    })
    .process(&path)
    .expect("process");
    assert!(
        data.features
            .iter()
            .all(|f| f.kind.layer_name() == "amenity")
    );
    assert_eq!(data.features.len(), 2, "café node and school way");
    for f in &data.features {
        assert!(
            data.tag_store
                .resolve_tags(&f.tags)
                .all(|(k, _)| k == "name")
        );
    }
}

#[test]
fn history_files_are_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("history.osh.pbf");
    let opts = PbfWriterOptions::new().required_feature("HistoricalInformation");
    PbfWriter::new(std::fs::File::create(&path).expect("create"), &opts)
        .expect("header")
        .finish()
        .expect("finish");
    let err = PbfProcessor::default()
        .process(&path)
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("history"), "{err}");
}

#[test]
fn missing_file_is_an_error() {
    let err = PbfProcessor::default()
        .process(Path::new("/definitely/not/here.osm.pbf"))
        .err()
        .expect("must fail");
    assert!(matches!(err, OsmError::Pbf { .. }), "{err:?}");
}

#[test]
fn element_filter_applies_to_raw_tags_before_classification() {
    let (_dir, path) = fixture("f.osm.pbf", true, false);
    let data = PbfProcessor::new(PipelineConfig {
        filter: Some(osmic_osm::TagFilter::parse("name=*").expect("valid")),
        tag_retention: TagRetention::Keys(vec!["amenity".to_string()].into()),
        ..Default::default()
    })
    .process(&path)
    .expect("process");
    let ids: Vec<OsmId> = data.features.iter().map(|f| f.id).collect();
    // Only the café and the named road carry `name`, even though `name`
    // itself is not retained.
    assert_eq!(ids, [OsmId::node(1), OsmId::way(10)]);
    let unnamed = PbfProcessor::new(PipelineConfig {
        filter: Some(osmic_osm::TagFilter::parse("!name").expect("valid")),
        ..Default::default()
    })
    .process(&path)
    .expect("process");
    assert!(
        unnamed
            .features
            .iter()
            .any(|f| f.id == OsmId::relation(100)),
        "relations are filtered too, and member ways are still cached"
    );
}

#[test]
fn locations_on_ways_with_missing_nodes_follow_the_incomplete_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("low.osm.pbf");
    let opts = PbfWriterOptions::new().sorted(true).locations_on_ways(true);
    let mut w =
        PbfWriter::new(std::fs::File::create(&path).expect("create"), &opts).expect("header");
    let (a, b, c) = (
        fc(-1_000_000_000, 400_000_000),
        fc(-999_990_000, 400_010_000),
        fc(-999_980_000, 400_000_000),
    );
    w.write_way_with_locations(
        1,
        &[1, 2, 3],
        &[Some(a), Some(b), Some(c)],
        &[("highway", "residential")],
        None,
    )
    .expect("way");
    // osmium add-locations-to-ways --ignore-missing-nodes: node 9 unknown.
    w.write_way_with_locations(
        2,
        &[1, 2, 9],
        &[Some(a), Some(b), None],
        &[("highway", "service")],
        None,
    )
    .expect("way");
    w.finish().expect("finish");

    let data = PbfProcessor::default().process(&path).expect("process");
    assert_eq!(data.stats.incomplete_ways, 1);
    let ids: Vec<OsmId> = data.features.iter().map(|f| f.id).collect();
    assert_eq!(
        ids,
        [OsmId::way(1)],
        "the way with a missing node is skipped"
    );
    assert!(
        data.bbox.max_lon <= -99.0 && data.bbox.max_lat <= 41.0,
        "no out-of-range coordinates: {:?}",
        data.bbox
    );
}
