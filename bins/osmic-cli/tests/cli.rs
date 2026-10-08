//! End-to-end tests of the `osmic` binary.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use assert_cmd::Command;
use predicates::prelude::*;

use osmic_core::{FixedCoord, OsmType};
use osmic_osm::pbf::{PbfWriter, PbfWriterOptions, read_header};

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

/// Signal handling: `serve` drains, other commands clean up and exit with
/// the shell's status for the signal.
#[cfg(unix)]
mod signals {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::process::{Child, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    use super::{fixture, osmic};

    /// The binary as a plain process, for tests that signal it.
    fn spawn_osmic(dir: &Path, args: &[&str]) -> Child {
        std::process::Command::new(env!("CARGO_BIN_EXE_osmic"))
            .env("RUST_LOG", "warn")
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn osmic")
    }

    /// Send `signal` (a `kill` name such as "TERM") to `child`.
    fn signal(child: &Child, signal: &str) {
        let sent = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(child.id().to_string())
            .status()
            .expect("kill");
        assert!(sent.success(), "kill -{signal}");
    }

    /// Wait up to `limit` for `child` to exit; kill it and fail otherwise.
    fn wait_for_exit(child: &mut Child, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait().expect("wait") {
                return status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("osmic did not exit within {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Status code of `GET path`, or `None` if the server does not answer.
    fn http_status(port: u16, path: &str) -> Option<u16> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).ok()?;
        line.split_whitespace().nth(1)?.parse().ok()
    }

    #[test]
    fn serve_drains_on_sigterm_and_exits_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pbf = fixture(dir.path());
        let archive = dir.path().join("sf.pmtiles");
        osmic()
            .args(["generate-tiles"])
            .arg(&pbf)
            .arg(&archive)
            .args(["--zoom", "14", "--tmp-dir"])
            .arg(dir.path())
            .assert()
            .success();
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let bind = format!("127.0.0.1:{port}");
        let mut server = spawn_osmic(
            dir.path(),
            &["serve", "sf.pmtiles", "--bind", &bind, "--drain-delay", "1"],
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        while http_status(port, "/readyz") != Some(200) {
            assert!(Instant::now() < deadline, "server never became ready");
            assert!(server.try_wait().expect("wait").is_none(), "server exited");
            std::thread::sleep(Duration::from_millis(50));
        }

        signal(&server, "TERM");
        // During the drain delay the server still answers, but is not ready.
        let deadline = Instant::now() + Duration::from_millis(900);
        let mut draining = false;
        while Instant::now() < deadline {
            if http_status(port, "/readyz") == Some(503) {
                draining = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(draining, "/readyz never reported 503 while draining");
        assert_eq!(http_status(port, "/healthz"), Some(200), "still serving");

        let status = wait_for_exit(&mut server, Duration::from_secs(20));
        assert_eq!(status.code(), Some(0), "clean drain exits 0: {status:?}");
    }

    #[test]
    fn interrupted_commands_remove_temporaries_and_exit_with_the_signal_status() {
        for (name, code) in [("INT", 130), ("TERM", 143)] {
            // A replication server that accepts the connection and never
            // answers, so `osmic update` is mid-run when the signal arrives.
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let server = format!("http://{}/", listener.local_addr().expect("addr"));
            let dir = tempfile::tempdir().expect("tempdir");
            fixture(dir.path());
            // A bare file name: its temporaries live in the current directory.
            let mut child = spawn_osmic(
                dir.path(),
                &[
                    "update",
                    "sf.osm.pbf",
                    "--server",
                    &server,
                    "--sequence",
                    "1",
                    "--allow-http",
                ],
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            let _connection = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(child.try_wait().expect("wait").is_none(), "update exited");
                        assert!(Instant::now() < deadline, "update never connected");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            // Stand-in for the temporary an in-place update writes.
            let temp = dir.path().join(format!(".osmic-{}-abc.tmp", child.id()));
            std::fs::write(&temp, b"partial").expect("temp");

            signal(&child, name);
            let status = wait_for_exit(&mut child, Duration::from_secs(20));
            assert_eq!(status.code(), Some(code), "SIG{name}: {status:?}");
            assert!(!temp.exists(), "SIG{name}: temporary left behind");
            assert!(dir.path().join("sf.osm.pbf").is_file());
        }
    }
}

/// Serve `files` (path → body) over HTTP until the test exits; returns the
/// base URL.
fn replication_server(files: HashMap<String, Vec<u8>>) -> String {
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
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let mut out = &stream;
                let _ = match files.get(path) {
                    Some(body) => write!(
                        out,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .and_then(|()| out.write_all(body)),
                    None => write!(
                        out,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                };
            });
        }
    });
    format!("http://{addr}/")
}

#[test]
fn update_applies_diffs_in_place() {
    let diff = r#"<osmChange version="0.6">
      <create><node id="50" version="1" lat="37.77" lon="-122.41"><tag k="amenity" v="cafe"/></node></create>
      <delete><node id="1" version="2"/></delete>
    </osmChange>"#;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(diff.as_bytes()).expect("gzip");
    let state = b"sequenceNumber=101\ntimestamp=2026-10-08T12\\:01\\:00Z\n".to_vec();
    let base = replication_server(HashMap::from([
        ("/state.txt".to_string(), state.clone()),
        (
            "/000/000/101.osc.gz".to_string(),
            gz.finish().expect("gzip"),
        ),
        ("/000/000/101.state.txt".to_string(), state),
    ]));

    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data");
    std::fs::create_dir(&data).expect("mkdir");
    let pbf = data.join("sf.osm.pbf");
    let opts = PbfWriterOptions {
        sorted: true,
        replication_sequence: Some(100),
        replication_timestamp: Some(1_791_460_800),
        replication_base_url: Some(base),
        ..Default::default()
    };
    let mut w =
        PbfWriter::new(std::fs::File::create(&pbf).expect("create"), &opts).expect("header");
    let f = |lon: f64, lat: f64| FixedCoord::from_degrees(lon, lat).expect("valid");
    w.write_node(1, f(-122.42, 37.77), &[("amenity", "cafe")])
        .expect("node");
    w.write_node(2, f(-122.43, 37.78), &[]).expect("node");
    w.finish().expect("finish");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pbf, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        // Updated through a relative symbolic link given as a bare name.
        std::os::unix::fs::symlink("data/sf.osm.pbf", dir.path().join("current.osm.pbf"))
            .expect("symlink");
    }
    let input = if cfg!(unix) {
        "current.osm.pbf"
    } else {
        "data/sf.osm.pbf"
    };

    osmic()
        .current_dir(dir.path())
        .args(["update", input, "--allow-http"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "sequence        100 -> 101 (server at 101)",
        ))
        .stdout(predicate::str::contains(
            "data as of      2026-10-08T12:01:00Z",
        ))
        .stdout(predicate::str::is_match(r"created\s+1\n").expect("regex"))
        .stdout(predicate::str::is_match(r"deleted\s+1\n").expect("regex"));

    let header = read_header(&pbf).expect("header");
    assert_eq!(header.replication_sequence, Some(101));
    let mut ids = Vec::new();
    osmpbf::ElementReader::from_path(&pbf)
        .expect("open")
        .for_each(|e| match e {
            osmpbf::Element::DenseNode(n) => ids.push(n.id()),
            osmpbf::Element::Node(n) => ids.push(n.id()),
            _ => {}
        })
        .expect("read");
    assert_eq!(ids, [2, 50]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let link = std::fs::symlink_metadata(dir.path().join("current.osm.pbf")).expect("link");
        assert!(link.file_type().is_symlink(), "the link was replaced");
        let mode = std::fs::metadata(&pbf)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644, "mode kept: {mode:o}");
    }
    // Nothing but the data (and the link) is left behind.
    assert_eq!(std::fs::read_dir(&data).expect("dir").count(), 1);
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("dir").count(),
        if cfg!(unix) { 2 } else { 1 }
    );

    // Already current: nothing to do, the file is left alone.
    osmic()
        .current_dir(dir.path())
        .args(["update", input, "--allow-http"])
        .assert()
        .success()
        .stdout(predicate::str::contains("101 -> 101"));
}
