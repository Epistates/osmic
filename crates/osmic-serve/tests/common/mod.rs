//! Shared fixtures: builds a small PMTiles archive on disk and drives the
//! router in-process.

#![allow(dead_code)]

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, header};
use http_body_util::BodyExt;
use osmic_serve::{TileServer, TileServerConfig};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileType};
use tower::ServiceExt;

pub const METADATA: &str = r#"{"name":"fixture","attribution":"(c) test","description":"d","vector_layers":[{"id":"roads","fields":{}}]}"#;

/// Payload stored for every fixture tile (uncompressed form).
pub fn payload(z: u8, x: u32, y: u32) -> Vec<u8> {
    format!("tile-{z}-{x}-{y}-").repeat(40).into_bytes()
}

/// Present tiles: 0/0/0, 1/0/0, 1/1/1, 2/1/1.
pub const PRESENT: [(u8, u32, u32); 4] = [(0, 0, 0), (1, 0, 0), (1, 1, 1), (2, 1, 1)];

pub struct Fixture {
    pub path: PathBuf,
    pub server: TileServer,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Fixture {
    pub fn router(&self) -> Router {
        self.server.router()
    }
}

fn unique_path() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "osmic-serve-test-{}-{}.pmtiles",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write an archive. `raw` tiles are stored verbatim (already in the
/// archive's compression), otherwise the writer compresses them.
pub fn write_archive(tile_type: TileType, compression: Compression, raw: bool) -> PathBuf {
    let path = unique_path();
    let mut writer = PmTilesWriter::new(tile_type)
        .tile_compression(compression)
        .min_zoom(0)
        .max_zoom(2)
        .bounds(-10.0, -5.0, 10.0, 5.0)
        .center(0.0, 0.0)
        .center_zoom(1)
        .metadata(METADATA)
        .create(File::create(&path).unwrap())
        .unwrap();
    for (z, x, y) in PRESENT {
        let coord = TileCoord::new(z, x, y).unwrap();
        let data = payload(z, x, y);
        if raw {
            writer.add_raw_tile(coord, &data).unwrap();
        } else {
            writer.add_tile(coord, &data).unwrap();
        }
    }
    writer.finalize().unwrap();
    path
}

pub async fn fixture_with(
    tile_type: TileType,
    compression: Compression,
    raw: bool,
    configure: impl FnOnce(TileServerConfig) -> TileServerConfig,
) -> Fixture {
    let path = write_archive(tile_type, compression, raw);
    let config = configure(TileServerConfig::new(&path).cache_max_age(120));
    let server = TileServer::open(config).await.unwrap();
    Fixture { path, server }
}

/// Default fixture: MVT, gzip-compressed tiles.
pub async fn fixture() -> Fixture {
    fixture_with(TileType::Mvt, Compression::Gzip, false, |c| c).await
}

pub async fn send(app: &Router, req: Request<Body>) -> Response<Body> {
    app.clone().oneshot(req).await.unwrap()
}

pub async fn get(app: &Router, uri: &str, headers: &[(&str, &str)]) -> Response<Body> {
    let mut req = Request::get(uri).header(header::HOST, "tiles.test:3000");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    send(app, req.body(Body::empty()).unwrap()).await
}

pub async fn body_bytes(res: Response<Body>) -> Vec<u8> {
    res.into_body().collect().await.unwrap().to_bytes().to_vec()
}

pub fn hdr<'a>(res: &'a Response<Body>, name: &str) -> &'a str {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
}

pub fn gunzip(data: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .unwrap();
    out
}
