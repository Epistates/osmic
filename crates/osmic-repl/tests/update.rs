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
    UpdateOptions {
        client: ClientOptions {
            allow_http: true,
            attempts: 1,
            ..ClientOptions::default()
        },
        ..UpdateOptions::default()
    }
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
    let mut opts = options();
    opts.client.max_diff_bytes = 1024;
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
