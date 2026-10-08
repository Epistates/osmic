mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;
use pmtiles::{Compression, TileType};
use serde_json::Value;

#[tokio::test]
async fn tile_gzip_passthrough() {
    let f = fixture().await;
    let res = get(&f.router(), "/tiles/1/0/0", &[("accept-encoding", "gzip")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        hdr(&res, "content-type"),
        "application/vnd.mapbox-vector-tile"
    );
    assert_eq!(hdr(&res, "content-encoding"), "gzip");
    assert_eq!(hdr(&res, "cache-control"), "public, max-age=120");
    assert_eq!(hdr(&res, "vary"), "accept-encoding");
    let etag = hdr(&res, "etag").to_owned();
    assert!(etag.starts_with('"') && etag.ends_with('"') && !etag.starts_with("W/"));
    assert_eq!(gunzip(&body_bytes(res).await), payload(1, 0, 0));
}

#[tokio::test]
async fn tile_identity_is_decompressed_with_distinct_etag() {
    let f = fixture().await;
    let app = f.router();
    let gz = get(&app, "/tiles/1/0/0", &[("accept-encoding", "gzip")]).await;
    let gz_etag = hdr(&gz, "etag").to_owned();

    for ae in [
        &[][..],
        &[("accept-encoding", "identity")],
        &[("accept-encoding", "gzip;q=0")],
    ] {
        let res = get(&app, "/tiles/1/0/0", ae).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("content-encoding").is_none());
        assert_eq!(hdr(&res, "vary"), "accept-encoding");
        assert_ne!(hdr(&res, "etag"), gz_etag);
        assert_eq!(body_bytes(res).await, payload(1, 0, 0));
    }
}

#[tokio::test]
async fn etag_is_stable_across_restarts() {
    let f = fixture().await;
    let a = get(&f.router(), "/tiles/0/0/0", &[]).await;
    let reopened = osmic_serve::TileServer::open(f.server.config().clone())
        .await
        .unwrap();
    let b = get(&reopened.router(), "/tiles/0/0/0", &[]).await;
    assert_eq!(hdr(&a, "etag"), hdr(&b, "etag"));
}

#[tokio::test]
async fn conditional_request_returns_304() {
    let f = fixture().await;
    let app = f.router();
    for ae in [("accept-encoding", "gzip"), ("accept-encoding", "identity")] {
        let first = get(&app, "/tiles/1/1/1", &[ae]).await;
        let etag = hdr(&first, "etag").to_owned();
        let res = get(&app, "/tiles/1/1/1", &[ae, ("if-none-match", &etag)]).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(hdr(&res, "etag"), etag);
        assert_eq!(hdr(&res, "cache-control"), "public, max-age=120");
        assert!(body_bytes(res).await.is_empty());
    }
    let res = get(&app, "/tiles/1/1/1", &[("if-none-match", "\"nope\"")]).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_tile_is_204_and_cacheable() {
    let f = fixture().await;
    let res = get(&f.router(), "/tiles/2/3/3", &[]).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(hdr(&res, "cache-control"), "public, max-age=120");
}

#[tokio::test]
async fn invalid_coordinates_are_400_no_store() {
    let f = fixture().await;
    let app = f.router();
    for uri in [
        "/tiles/1/2/0",
        "/tiles/1/0/2",
        "/tiles/40/0/0",
        "/tiles/a/b/c",
        "/tiles/1/-1/0",
        "/tiles/1/0/0.png",
    ] {
        let res = get(&app, uri, &[]).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(hdr(&res, "cache-control"), "no-store", "{uri}");
    }
}

#[tokio::test]
async fn zoom_above_max_is_404() {
    let f = fixture().await;
    let res = get(&f.router(), "/tiles/3/0/0", &[]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(hdr(&res, "cache-control"), "no-store");
}

#[tokio::test]
async fn suffix_routes_serve_same_tile() {
    let f = fixture().await;
    let app = f.router();
    let plain = body_bytes(get(&app, "/tiles/2/1/1", &[]).await).await;
    assert_eq!(plain, payload(2, 1, 1));
    for ext in ["mvt", "pbf", "mlt"] {
        let res = get(&app, &format!("/tiles/2/1/1.{ext}"), &[]).await;
        assert_eq!(res.status(), StatusCode::OK, "{ext}");
        assert_eq!(body_bytes(res).await, plain, "{ext}");
    }
}

#[tokio::test]
async fn head_requests_work() {
    let f = fixture().await;
    let req = Request::head("/tiles/0/0/0").body(Body::empty()).unwrap();
    let res = send(&f.router(), req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key("etag"));
    assert!(body_bytes(res).await.is_empty());
}

#[tokio::test]
async fn content_type_follows_tile_type() {
    for (tt, expected) in [
        (TileType::Mlt, "application/vnd.maplibre-vector-tile"),
        (TileType::Png, "image/png"),
        (TileType::Jpeg, "image/jpeg"),
        (TileType::Webp, "image/webp"),
        (TileType::Avif, "image/avif"),
    ] {
        let f = fixture_with(tt, Compression::None, false, |c| c).await;
        let res = get(&f.router(), "/tiles/0/0/0", &[("accept-encoding", "gzip")]).await;
        assert_eq!(hdr(&res, "content-type"), expected);
        assert!(res.headers().get("content-encoding").is_none());
        assert_eq!(body_bytes(res).await, payload(0, 0, 0));
        let tj: Value =
            serde_json::from_slice(&body_bytes(get(&f.router(), "/tiles.json", &[]).await).await)
                .unwrap();
        assert_eq!(tj.get("vector_layers").is_some(), tt == TileType::Mlt);
    }
}

#[tokio::test]
async fn brotli_archive_passes_through_only_when_accepted() {
    let f = fixture_with(TileType::Mvt, Compression::Brotli, true, |c| c).await;
    let app = f.router();
    let ok = get(&app, "/tiles/0/0/0", &[("accept-encoding", "gzip, br")]).await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(hdr(&ok, "content-encoding"), "br");
    let bad = get(&app, "/tiles/0/0/0", &[("accept-encoding", "gzip")]).await;
    assert_eq!(bad.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(hdr(&bad, "cache-control"), "no-store");
}

#[tokio::test]
async fn unknown_compression_is_500_no_store() {
    let f = fixture_with(TileType::Mvt, Compression::Unknown, true, |c| c).await;
    let res = get(&f.router(), "/tiles/0/0/0", &[("accept-encoding", "gzip")]).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(hdr(&res, "cache-control"), "no-store");
}

#[tokio::test]
async fn tilejson_shape() {
    let f = fixture_with(TileType::Mvt, Compression::Gzip, false, |c| {
        c.public_url("https://t.example.com/")
    })
    .await;
    let res = get(&f.router(), "/tiles.json", &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hdr(&res, "content-type"), "application/json");
    assert_eq!(hdr(&res, "cache-control"), "public, max-age=60");
    let v: Value = serde_json::from_slice(&body_bytes(res).await).unwrap();
    assert_eq!(v["tilejson"], "3.0.0");
    assert_eq!(v["tiles"][0], "https://t.example.com/tiles/{z}/{x}/{y}.mvt");
    assert_eq!(v["minzoom"], 0);
    assert_eq!(v["maxzoom"], 2);
    assert_eq!(v["bounds"], serde_json::json!([-10.0, -5.0, 10.0, 5.0]));
    assert_eq!(v["center"], serde_json::json!([0.0, 0.0, 1]));
    assert_eq!(v["attribution"], "(c) test");
    assert_eq!(v["vector_layers"][0]["id"], "roads");
}

#[tokio::test]
async fn metadata_is_raw_archive_metadata() {
    let f = fixture().await;
    let res = get(&f.router(), "/metadata", &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let v: Value = serde_json::from_slice(&body_bytes(res).await).unwrap();
    assert_eq!(v["name"], "fixture");
}

#[tokio::test]
async fn style_urls_from_public_url() {
    let f = fixture_with(TileType::Mvt, Compression::Gzip, false, |c| {
        c.public_url("https://t.example.com/base")
    })
    .await;
    let res = get(&f.router(), "/style.json", &[("host", "evil.example")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()
            .get("vary")
            .is_none_or(|v| !v.to_str().unwrap().contains("host"))
    );
    let v: Value = serde_json::from_slice(&body_bytes(res).await).unwrap();
    assert_eq!(
        v["sources"]["osmic"]["url"],
        "https://t.example.com/base/tiles.json"
    );
}

#[tokio::test]
async fn style_urls_from_host_header() {
    let f = fixture().await;
    let res = get(&f.router(), "/style.json", &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(hdr(&res, "vary").to_ascii_lowercase().contains("host"));
    let v: Value = serde_json::from_slice(&body_bytes(res).await).unwrap();
    assert_eq!(
        v["sources"]["osmic"]["url"],
        "http://tiles.test:3000/tiles.json"
    );
}

#[tokio::test]
async fn malicious_host_is_rejected() {
    let f = fixture().await;
    let app = f.router();
    for host in [
        "evil.com/\"><script>",
        "a b",
        "x\"y",
        "user@evil.com",
        "evil.com:80:90",
        "a.com/path",
        "<b>",
    ] {
        for path in ["/style.json", "/tiles.json"] {
            let req = Request::get(path)
                .header(header::HOST, host)
                .body(Body::empty());
            // Some of these are not even representable as header values.
            let Ok(req) = req else { continue };
            let res = send(&app, req).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{host} {path}");
            assert_eq!(hdr(&res, "cache-control"), "no-store");
        }
    }
}

#[tokio::test]
async fn viewer_is_pinned_sri_and_csp() {
    let f = fixture().await;
    let app = f.router();
    let res = get(&app, "/", &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let csp = hdr(&res, "content-security-policy").to_owned();
    assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
    let html = String::from_utf8(body_bytes(res).await).unwrap();
    assert!(html.contains("maplibre-gl@5.3.0/dist/maplibre-gl.js"));
    assert_eq!(html.matches("integrity=\"sha384-").count(), 2);
    assert_eq!(html.matches("crossorigin=\"anonymous\"").count(), 2);
    assert!(!html.contains("<script>"), "no inline script");
    assert!(html.contains("data-config="));
    for asset in ["/viewer.js", "/viewer.css"] {
        assert_eq!(get(&app, asset, &[]).await.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn health_and_readiness() {
    let f = fixture().await;
    let app = f.router();
    for p in ["/healthz", "/readyz"] {
        let res = get(&app, p, &[]).await;
        assert_eq!(res.status(), StatusCode::OK, "{p}");
        assert_eq!(hdr(&res, "cache-control"), "no-store");
    }
}

#[tokio::test]
async fn error_responses_are_never_cacheable() {
    let f = fixture().await;
    let app = f.router();
    let res = get(&app, "/nope", &[]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(hdr(&res, "cache-control"), "no-store");
    let req = Request::post("/tiles/0/0/0").body(Body::empty()).unwrap();
    let res = send(&app, req).await;
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(hdr(&res, "cache-control"), "no-store");
}

fn preflight(origin: &str, method: &str) -> Request<Body> {
    Request::builder()
        .method(Method::OPTIONS)
        .uri("/tiles/0/0/0")
        .header(header::ORIGIN, origin)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, method)
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "if-none-match")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn cors_defaults_allow_any_origin_get_only() {
    let f = fixture().await;
    let app = f.router();
    let res = send(&app, preflight("https://app.example", "GET")).await;
    assert!(res.status().is_success());
    assert_eq!(hdr(&res, "access-control-allow-origin"), "*");
    let methods = hdr(&res, "access-control-allow-methods").to_owned();
    assert!(methods.contains("GET") && methods.contains("HEAD"));
    assert!(!methods.contains("POST") && !methods.contains("DELETE"));
    assert!(
        res.headers()
            .get("access-control-allow-credentials")
            .is_none()
    );
    assert!(
        hdr(&res, "access-control-allow-headers")
            .to_ascii_lowercase()
            .contains("if-none-match")
    );

    // Actual requests expose the validator.
    let res = get(&app, "/tiles/0/0/0", &[("origin", "https://app.example")]).await;
    assert_eq!(hdr(&res, "access-control-allow-origin"), "*");
    assert!(
        hdr(&res, "access-control-expose-headers")
            .to_ascii_lowercase()
            .contains("etag")
    );
}

#[tokio::test]
async fn cors_origin_allowlist() {
    let f = fixture_with(TileType::Mvt, Compression::Gzip, false, |c| {
        c.cors_allowed_origins(["https://app.example"])
    })
    .await;
    let app = f.router();
    let ok = send(&app, preflight("https://app.example", "GET")).await;
    assert_eq!(
        hdr(&ok, "access-control-allow-origin"),
        "https://app.example"
    );
    let other = send(&app, preflight("https://evil.example", "GET")).await;
    assert!(other.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn serve_with_shutdown_drains_and_stops() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let server = osmic_serve::TileServer::open(f.server.config().clone())
            .await
            .unwrap();
        server
            .serve_with_shutdown(listener, async {
                let _ = rx.await;
            })
            .await
    });
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    assert!(buf.starts_with("HTTP/1.1 200"), "{buf}");
    tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("server stops after shutdown signal")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn open_errors_are_typed() {
    use osmic_serve::{ServeError, TileServer, TileServerConfig};
    let err = TileServer::open(TileServerConfig::new("/nonexistent/x.pmtiles"))
        .await
        .unwrap_err();
    assert!(matches!(err, ServeError::ArchiveNotFound(_)));
    let err = TileServer::open(TileServerConfig::new("x").max_concurrency(0))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ServeError::InvalidConfig {
            field: "max_concurrency",
            ..
        }
    ));
    // A file that exists but is not a PMTiles archive.
    let junk = std::env::temp_dir().join(format!("osmic-serve-junk-{}", std::process::id()));
    std::fs::write(
        &junk,
        b"not a pmtiles archive, definitely not 127 bytes of valid header",
    )
    .unwrap();
    let err = TileServer::open(TileServerConfig::new(&junk))
        .await
        .unwrap_err();
    let _ = std::fs::remove_file(&junk);
    assert!(matches!(err, ServeError::Archive { .. }));
    assert!(std::error::Error::source(&err).is_some());
}

#[tokio::test]
async fn extra_routes_are_served_behind_the_middleware() {
    use axum::routing::get as get_route;
    let f = fixture().await;
    let mut routes = osmic_serve::ServerRoutes::default();
    routes.nest(
        "/api",
        axum::Router::new().route("/version", get_route(|| async { "1.0" })),
    );
    let server = osmic_serve::TileServer::open(f.server.config().clone())
        .await
        .unwrap()
        .with_routes(routes)
        .unwrap();
    let app = server.router();
    let res = get(&app, "/api/version", &[("origin", "https://a.example")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hdr(&res, "x-content-type-options"), "nosniff");
    assert_eq!(hdr(&res, "access-control-allow-origin"), "*");
    assert_eq!(&body_bytes(res).await[..], b"1.0");
    // Built-in endpoints are unaffected.
    let tile = get(&app, "/tiles/0/0/0", &[]).await;
    assert_eq!(tile.status(), StatusCode::OK);

    let mut shadowing = osmic_serve::ServerRoutes::default();
    shadowing.nest("/tiles", axum::Router::new());
    let err = osmic_serve::TileServer::open(f.server.config().clone())
        .await
        .unwrap()
        .with_routes(shadowing)
        .expect_err("shadows /tiles");
    assert!(err.to_string().contains("built-in"), "{err}");
}
