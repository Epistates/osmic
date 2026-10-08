//! End-to-end tests of the `osmic` binary.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

use osmic_core::{FixedCoord, OsmType};
use osmic_osm::pbf::{PbfWriter, PbfWriterOptions};

fn osmic() -> Command {
    let mut c = Command::cargo_bin("osmic").expect("binary");
    c.env("RUST_LOG", "warn");
    c
}

/// A small San Francisco dataset: a café, a road, a park with a hole.
fn fixture(dir: &Path) -> PathBuf {
    let path = dir.join("sf.osm.pbf");
    let opts = PbfWriterOptions::new().sorted(true);
    let mut w =
        PbfWriter::new(std::fs::File::create(&path).expect("create"), &opts).expect("header");
    let f = |lon: f64, lat: f64| FixedCoord::from_degrees(lon, lat).expect("valid");
    w.write_node(
        1,
        f(-122.4194, 37.7749),
        &[
            ("amenity", "cafe"),
            ("name", "=Blue Bottle"),
            ("phone", "+1 555"),
        ],
    )
    .expect("node");
    for (i, (lon, lat)) in [
        (-122.42, 37.77),
        (-122.41, 37.77),
        (-122.43, 37.76),
        (-122.40, 37.76),
        (-122.40, 37.78),
        (-122.43, 37.78),
        (-122.42, 37.765),
        (-122.41, 37.765),
        (-122.41, 37.775),
        (-122.42, 37.775),
    ]
    .into_iter()
    .enumerate()
    {
        w.write_node(10 + i as i64, f(lon, lat), &[]).expect("node");
    }
    w.write_way(
        100,
        &[10, 11],
        &[("highway", "primary"), ("name", "Market St")],
    )
    .expect("way");
    w.write_way(101, &[12, 13, 14, 15, 12], &[]).expect("way");
    w.write_way(102, &[16, 17, 18, 19, 16], &[]).expect("way");
    w.write_relation(
        200,
        &[(OsmType::Way, 101, "outer"), (OsmType::Way, 102, "inner")],
        &[
            ("type", "multipolygon"),
            ("leisure", "park"),
            ("name", "Park"),
        ],
    )
    .expect("relation");
    w.finish().expect("finish");
    path
}

#[test]
fn version_and_help() {
    osmic()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("osmic "));
    osmic()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("generate-tiles"));
}

#[test]
fn generate_tiles_writes_a_readable_archive_and_style() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = fixture(dir.path());
    let out = dir.path().join("sf.pmtiles");
    let style = dir.path().join("style.json");
    osmic()
        .args(["generate-tiles"])
        .arg(&pbf)
        .arg(&out)
        .args(["--zoom", "10-14", "--style"])
        .arg(&style)
        .args(["--tmp-dir"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("tiles"));
    let bytes = std::fs::read(&out).expect("archive");
    let header =
        pmtiles::Header::try_from_bytes(bytes::Bytes::from(bytes)).expect("pmtiles header");
    assert!(header.clustered());
    assert_eq!((header.min_zoom, header.max_zoom), (10, 14));
    let style: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&style).expect("style")).expect("json");
    assert!(style["sources"].is_object());
    // Refuses to overwrite without --force …
    osmic()
        .args(["generate-tiles"])
        .arg(&pbf)
        .arg(&out)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--force"));
    // … and succeeds with it.
    osmic()
        .args(["generate-tiles"])
        .arg(&pbf)
        .arg(&out)
        .args(["--zoom", "12", "--force", "--tmp-dir"])
        .arg(dir.path())
        .assert()
        .success();
    // No temporary files left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".osmic-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn tag_filters_limit_tile_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = fixture(dir.path());
    let out = dir.path().join("cafes.pmtiles");
    osmic()
        .args(["generate-tiles"])
        .arg(&pbf)
        .arg(&out)
        .args(["--zoom", "14", "--tags", "amenity=cafe", "--tmp-dir"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"features\s+1\n").expect("regex"));
}

#[test]
fn inspect_reports_counts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = fixture(dir.path());
    osmic()
        .arg("inspect")
        .arg(&pbf)
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"nodes\s+11").expect("regex"))
        .stdout(predicate::str::contains("assembled 1"));
}

#[test]
fn extract_every_format_with_safe_csv() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = fixture(dir.path());
    for name in ["out.csv", "out.json", "out.geojson"] {
        let out = dir.path().join(name);
        osmic()
            .arg("extract")
            .arg(&pbf)
            .arg(&out)
            .args(["--all-tags", "--exclude-tags", "highway=*"])
            .assert()
            .success();
        assert!(out.is_file());
    }
    let csv = std::fs::read_to_string(dir.path().join("out.csv")).expect("csv");
    assert!(csv.contains("'=Blue Bottle"), "formula neutralised: {csv}");
    assert!(
        csv.contains("Park"),
        "relation located and extracted: {csv}"
    );
    assert!(!csv.contains("Market St"), "excluded by --exclude-tags");
}

#[test]
fn usage_and_runtime_errors_have_distinct_exit_codes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = fixture(dir.path());
    // Usage errors (clap): exit 2.
    osmic()
        .args(["generate-tiles"])
        .arg(&pbf)
        .arg(dir.path().join("x.pmtiles"))
        .args(["--zoom", "14-0"])
        .assert()
        .code(2);
    osmic()
        .args(["extract"])
        .arg(&pbf)
        .arg(dir.path().join("x.csv"))
        .args(["--bbox", "1,2,3"])
        .assert()
        .code(2);
    osmic()
        .args(["extract"])
        .arg(&pbf)
        .arg(dir.path().join("x.csv"))
        .args(["--tags", "=bad"])
        .assert()
        .code(2);
    // Runtime errors: exit 1 with a message.
    osmic()
        .args(["inspect", "/definitely/missing.osm.pbf"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("does not exist"));
    osmic()
        .args(["extract"])
        .arg(&pbf)
        .arg(dir.path().join("x.unknown"))
        .arg("--all-tags")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--format"));
}
