//! End-to-end replication against a local HTTP server.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;

use osmic_core::{FixedCoord, OsmType};
use osmic_osm::pbf::{PbfWriter, PbfWriterOptions, read_header};
use osmic_repl::{ClientOptions, ReplError, UpdateOptions, update_pbf};

/// Serve `files` (path → body) until the process exits; returns the base URL.
fn serve(files: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let files = Arc::new(files);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let files = Arc::clone(&files);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    return;
                }
                // Drain headers.
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut out = &stream;
                let _ = match files.get(&path) {
                    Some(body) => {
                        let _ = write!(
                            out,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        out.write_all(body)
                    }
                    None => write!(
                        out,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                };
            });
        }
    });
    format!("http://{addr}")
}

fn gz(text: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(text.as_bytes()).expect("gzip");
    e.finish().expect("gzip")
}

fn write_base(path: &Path, base_url: &str) {
    let opts = PbfWriterOptions::new().sorted(true).replication(
        Some(1_791_460_800),
        Some(100),
        Some(base_url.to_string()),
    );
    let mut w =
        PbfWriter::new(std::fs::File::create(path).expect("create"), &opts).expect("header");
    for (id, lon) in [(1, 10), (2, 20), (3, 30)] {
        w.write_node(id, FixedCoord::new(lon, 0), &[])
            .expect("node");
    }
    w.write_way(10, &[1, 2], &[("highway", "residential")])
        .expect("way");
    w.write_way(11, &[2, 3], &[("highway", "service")])
        .expect("way");
    w.write_relation(20, &[(OsmType::Way, 10, "outer")], &[("type", "route")])
        .expect("relation");
    w.finish().expect("finish");
}

#[derive(Debug, PartialEq)]
enum Item {
    Node(i64, i32),
    Way(i64, Vec<i64>),
    Relation(i64),
}

fn read_items(path: &Path) -> Vec<Item> {
    let mut out = Vec::new();
    osmpbf::ElementReader::from_path(path)
        .expect("open")
        .for_each(|e| match e {
            osmpbf::Element::DenseNode(n) => out.push(Item::Node(n.id(), n.decimicro_lon())),
            osmpbf::Element::Node(n) => out.push(Item::Node(n.id(), n.decimicro_lon())),
            osmpbf::Element::Way(w) => out.push(Item::Way(w.id(), w.refs().collect())),
            osmpbf::Element::Relation(r) => out.push(Item::Relation(r.id())),
        })
        .expect("read");
    out
}

fn options() -> UpdateOptions {
    UpdateOptions::default().client(ClientOptions::default().allow_http(true).attempts(1))
}

#[test]
fn catches_up_through_several_diffs_in_place() {
    let diff101 = r#"<osmChange version="0.6">
      <modify><node id="2" version="2" lat="0" lon="0.0000025"/></modify>
      <create><node id="50" version="1" lat="0" lon="0.0000099"/></create>
      <delete><way id="11" version="2"/></delete>
    </osmChange>"#;
    let diff102 = r#"<osmChange version="0.6">
      <create><way id="12" version="1"><nd ref="1"/><nd ref="50"/><tag k="highway" v="path"/></way></create>
      <modify><node id="50" version="2" lat="0" lon="0.0000051"/></modify>
      <delete><relation id="20" version="3"/></delete>
    </osmChange>"#;
    let mut files = HashMap::new();
    files.insert(
        "/state.txt".into(),
        b"sequenceNumber=102\ntimestamp=2026-10-08T12\\:02\\:00Z\n".to_vec(),
    );
    files.insert("/000/000/101.osc.gz".into(), gz(diff101));
    files.insert("/000/000/102.osc.gz".into(), gz(diff102));
    files.insert(
        "/000/000/102.state.txt".into(),
        b"sequenceNumber=102\ntimestamp=2026-10-08T12\\:02\\:00Z\n".to_vec(),
    );
    let base = serve(files);

    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    write_base(&pbf, &base);

    let report = update_pbf(&pbf, &pbf, &options()).expect("update");
    assert_eq!((report.from.sequence, report.to.sequence), (100, 102));
    assert_eq!(report.diffs_applied, 2);
    assert!(report.up_to_date());
    assert_eq!(
        (
            report.stats.created,
            report.stats.modified,
            report.stats.deleted
        ),
        (2, 1, 2)
    );

    assert_eq!(
        read_items(&pbf),
        [
            Item::Node(1, 10),
            Item::Node(2, 25),
            Item::Node(3, 30),
            Item::Node(50, 51),
            Item::Way(10, vec![1, 2]),
            Item::Way(12, vec![1, 50]),
        ]
    );
    let header = read_header(&pbf).expect("header");
    assert!(header.is_sorted());
    assert_eq!(header.replication_sequence, Some(102));
    assert_eq!(header.replication_timestamp, Some(1_791_460_920));
    // No temporary files left next to the data.
    assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 1);

    // A second run has nothing to do and does not rewrite the file.
    let before = std::fs::read(&pbf).expect("read");
    let again = update_pbf(&pbf, &pbf, &options()).expect("update");
    assert_eq!(again.diffs_applied, 0);
    assert_eq!(std::fs::read(&pbf).expect("read"), before);
}

#[test]
fn http_is_refused_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    write_base(&pbf, "http://127.0.0.1:9/");
    let err = update_pbf(&pbf, &pbf, &UpdateOptions::default()).expect_err("http must fail");
    assert!(
        matches!(err, ReplError::State(ref m) if m.contains("https")),
        "{err}"
    );
}

#[test]
fn oversized_diffs_are_rejected() {
    let mut files = HashMap::new();
    files.insert("/state.txt".into(), b"sequenceNumber=101\n".to_vec());
    files.insert("/000/000/101.osc.gz".into(), vec![0u8; 4096]);
    let base = serve(files);
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    write_base(&pbf, &base);
    let opts = UpdateOptions::default().client(
        ClientOptions::default()
            .allow_http(true)
            .attempts(1)
            .max_diff_bytes(1024),
    );
    let err = update_pbf(&pbf, &pbf, &opts).expect_err("too large");
    assert!(matches!(err, ReplError::TooLarge { .. }), "{err}");
}

#[test]
fn unpublished_next_diff_stops_cleanly() {
    let mut files = HashMap::new();
    // The server announces 103 but only 101 is downloadable so far.
    files.insert("/state.txt".into(), b"sequenceNumber=103\n".to_vec());
    files.insert(
        "/000/000/101.osc.gz".into(),
        gz(r#"<osmChange><delete><node id="3"/></delete></osmChange>"#),
    );
    let base = serve(files);
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    let out = dir.path().join("updated.osm.pbf");
    write_base(&pbf, &base);
    let report = update_pbf(&pbf, &out, &options()).expect("update");
    assert_eq!(report.to.sequence, 101);
    assert!(!report.up_to_date());
    assert_eq!(
        read_header(&out).expect("header").replication_sequence,
        Some(101)
    );
    assert!(!read_items(&out).contains(&Item::Node(3, 30)));
}

fn state(seq: u64) -> Vec<u8> {
    format!("sequenceNumber={seq}\ntimestamp=2026-10-08T12\\:02\\:00Z\n").into_bytes()
}

#[test]
fn untouched_blocks_and_metadata_are_preserved() {
    use osmic_osm::pbf::ElementMeta;
    let mut files = HashMap::new();
    files.insert("/state.txt".into(), state(101));
    files.insert(
        "/000/000/101.osc.gz".into(),
        gz(r#"<osmChange><modify><way id="10" version="9" timestamp="2026-10-08T12:00:00Z" changeset="77" uid="3" user="ed"><nd ref="1"/><nd ref="3"/><tag k="highway" v="primary"/></way></modify></osmChange>"#),
    );
    files.insert("/000/000/101.state.txt".into(), state(101));
    let base = serve(files);
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    // Nodes with metadata in their own (untouched) block.
    let opts = PbfWriterOptions::new().sorted(true).replication(
        Some(1_791_460_800),
        Some(100),
        Some(base.clone()),
    );
    let mut w =
        PbfWriter::new(std::fs::File::create(&pbf).expect("create"), &opts).expect("header");
    let meta = ElementMeta {
        version: 4,
        timestamp: 1_700_000_000,
        changeset: 5,
        uid: 6,
        user: "mapper".into(),
    };
    for id in 1..=3 {
        w.write_node_with_meta(
            id,
            FixedCoord::new(id as i32, 0),
            &[("a", "b")],
            Some(&meta),
        )
        .expect("node");
    }
    w.write_way(10, &[1, 2], &[("highway", "residential")])
        .expect("way");
    w.finish().expect("finish");

    let report = update_pbf(&pbf, &pbf, &options()).expect("update");
    assert_eq!(
        (report.stats.blocks_copied, report.stats.blocks_rewritten),
        (1, 1)
    );
    assert_eq!(report.stats.modified, 1);

    let mut nodes = 0;
    let mut way_meta = None;
    osmpbf::ElementReader::from_path(&pbf)
        .expect("open")
        .for_each(|e| match e {
            osmpbf::Element::DenseNode(n) => {
                let i = n.info().expect("metadata kept");
                assert_eq!(
                    (i.version(), i.uid(), i.user().expect("utf8")),
                    (4, 6, "mapper")
                );
                assert_eq!(n.tags().collect::<Vec<_>>(), [("a", "b")]);
                nodes += 1;
            }
            osmpbf::Element::Way(w) => {
                assert_eq!(w.refs().collect::<Vec<_>>(), [1, 3]);
                let i = w.info();
                way_meta = Some((i.version(), i.changeset(), i.uid(), i.milli_timestamp()));
            }
            _ => {}
        })
        .expect("read");
    assert_eq!(nodes, 3);
    assert_eq!(
        way_meta,
        Some((Some(9), Some(77), Some(3), Some(1_791_460_800_000)))
    );
}

#[test]
fn state_files_must_match_the_requested_sequence() {
    let mut files = HashMap::new();
    files.insert("/state.txt".into(), state(101));
    files.insert(
        "/000/000/101.osc.gz".into(),
        gz(r#"<osmChange><delete><node id="3"/></delete></osmChange>"#),
    );
    // The per-sequence state lies about where it is.
    files.insert("/000/000/101.state.txt".into(), state(5_000));
    let base = serve(files);
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    write_base(&pbf, &base);
    let err = update_pbf(&pbf, &pbf, &options()).expect_err("mismatch");
    assert!(
        matches!(err, ReplError::State(ref m) if m.contains("5000")),
        "{err}"
    );
    assert_eq!(
        read_header(&pbf).expect("header").replication_sequence,
        Some(100),
        "untouched"
    );
}

#[test]
fn error_pages_are_not_applied_as_empty_diffs() {
    let mut files = HashMap::new();
    files.insert("/state.txt".into(), state(101));
    files.insert(
        "/000/000/101.osc.gz".into(),
        gz("<html><body>502 Bad Gateway</body></html>"),
    );
    let base = serve(files);
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    write_base(&pbf, &base);
    let err = update_pbf(&pbf, &pbf, &options()).expect_err("not a diff");
    assert!(
        matches!(err, ReplError::Osc(ref m) if m.contains("diff 101")),
        "{err}"
    );
}

#[test]
fn plain_diff_bodies_are_accepted() {
    // A server that sent the .osc.gz with Content-Encoding: gzip arrives
    // already inflated.
    let mut files = HashMap::new();
    files.insert("/state.txt".into(), state(101));
    files.insert(
        "/000/000/101.osc.gz".into(),
        br#"<osmChange><delete><node id="3"/></delete></osmChange>"#.to_vec(),
    );
    let base = serve(files);
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    write_base(&pbf, &base);
    let report = update_pbf(&pbf, &pbf, &options()).expect("update");
    assert_eq!(report.stats.deleted, 1);
}

#[test]
fn another_stream_needs_an_explicit_sequence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    write_base(&pbf, "http://127.0.0.1:9/daily/");
    let opts = options().server(Some("http://127.0.0.1:9/minute/".into()));
    let err = update_pbf(&pbf, &pbf, &opts).expect_err("needs a sequence");
    assert!(
        matches!(err, ReplError::State(ref m) if m.contains("starting sequence")),
        "{err}"
    );
}

#[test]
fn output_is_written_even_when_already_current() {
    let mut files = HashMap::new();
    files.insert("/state.txt".into(), state(100));
    let base = serve(files);
    let dir = tempfile::tempdir().expect("tempdir");
    let pbf = dir.path().join("data.osm.pbf");
    let out = dir.path().join("out.osm.pbf");
    write_base(&pbf, &base);
    let report = update_pbf(&pbf, &out, &options()).expect("update");
    assert_eq!(report.diffs_applied, 0);
    assert_eq!(
        std::fs::read(&out).expect("output"),
        std::fs::read(&pbf).expect("input")
    );
}
