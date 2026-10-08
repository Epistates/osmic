//! Request handlers.
//!
//! Caching policy (see also the crate docs):
//!
//! * Tiles (`200` and `204`): `public, max-age=<cache_max_age>`. The archive
//!   is immutable while the server runs, so an absent tile stays absent and
//!   `204` is cacheable as well.
//! * JSON / HTML / assets: `public, max-age=60`.
//! * Health probes and every `4xx`/`5xx`: `no-store`.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use pmtiles::{Compression, TileCoord};
use twox_hash::XxHash3_64;

use crate::archive::{Archive, VIEWER_CSP};
use crate::http::{
    NO_STORE, SHORT_CACHE, accepts_encoding, if_none_match_hits, is_valid_authority,
};

/// Upper bound for a decompressed tile; guards against decompression bombs.
const MAX_DECOMPRESSED_TILE: u64 = 64 * 1024 * 1024;
/// Tiles at least this large are inflated on the blocking pool.
const BLOCKING_DECOMPRESS_THRESHOLD: usize = 64 * 1024;

pub(crate) struct AppState {
    pub archive: Archive,
    pub tile_cache_control: HeaderValue,
    pub public_url: Option<String>,
    /// Set once shutdown begins so `/readyz` fails and load balancers drain us.
    pub draining: AtomicBool,
}

/// Failure of a single request. Always rendered with `Cache-Control: no-store`.
#[derive(Debug)]
pub(crate) enum ApiError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        };
        (
            status,
            [
                (header::CACHE_CONTROL, NO_STORE),
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            ],
            msg,
        )
            .into_response()
    }
}

fn header_value(s: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(s).map_err(|e| {
        tracing::error!(error = %e, value = s, "invalid header value");
        ApiError::Internal
    })
}

/// Parse `{y}` with an optional `.mvt` / `.pbf` / `.mlt` suffix.
fn parse_coord(z: &str, x: &str, y: &str) -> Result<TileCoord, ApiError> {
    const BAD: ApiError = ApiError::BadRequest("invalid tile coordinates");
    let y = [".mvt", ".pbf", ".mlt"]
        .iter()
        .find_map(|ext| y.strip_suffix(ext))
        .unwrap_or(y);
    let z: u8 = z.parse().map_err(|_| BAD)?;
    let x: u32 = x.parse().map_err(|_| BAD)?;
    let y: u32 = y.parse().map_err(|_| BAD)?;
    TileCoord::new(z, x, y).map_err(|_| BAD)
}

/// `GET /tiles/{z}/{x}/{y}[.mvt|.pbf|.mlt]`
pub(crate) async fn get_tile(
    State(state): State<std::sync::Arc<AppState>>,
    Path((z, x, y)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let coord = parse_coord(&z, &x, &y)?;
    let archive = &state.archive;
    if coord.z() > archive.max_zoom {
        return Err(ApiError::NotFound("zoom level not available"));
    }

    let raw = archive.reader().get_tile(coord).await.map_err(|e| {
        tracing::error!(z = coord.z(), x = coord.x(), y = coord.y(), error = %e, "tile read failed");
        ApiError::Internal
    })?;
    let Some(raw) = raw else {
        return Ok((
            StatusCode::NO_CONTENT,
            [
                (header::CACHE_CONTROL, state.tile_cache_control.clone()),
                (header::VARY, HeaderValue::from_static("accept-encoding")),
            ],
        )
            .into_response());
    };

    let representation = choose_representation(archive.tile_compression, &headers)?;
    // Strong validator over the stored bytes; the identity representation of a
    // compressed tile gets a distinct suffix because its bytes differ.
    let hash = XxHash3_64::oneshot(&raw);
    let etag = match representation {
        Representation::Decompress => format!("\"{hash:016x}-id\""),
        _ => format!("\"{hash:016x}\""),
    };
    let etag = header_value(&etag)?;

    let mut out = HeaderMap::with_capacity(6);
    out.insert(header::CACHE_CONTROL, state.tile_cache_control.clone());
    out.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    out.insert(header::ETAG, etag.clone());

    if let Ok(etag_str) = etag.to_str()
        && if_none_match_hits(&headers, etag_str)
    {
        return Ok((StatusCode::NOT_MODIFIED, out).into_response());
    }

    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(archive.tile_type.content_type()),
    );
    let body = match representation {
        Representation::Identity => raw,
        Representation::Passthrough(enc) => {
            out.insert(header::CONTENT_ENCODING, HeaderValue::from_static(enc));
            raw
        }
        Representation::Decompress => gunzip(raw).await?,
    };
    Ok((StatusCode::OK, out, body).into_response())
}

enum Representation {
    /// Stored uncompressed; send as is.
    Identity,
    /// Stored compressed with an encoding the client accepts; send as is.
    Passthrough(&'static str),
    /// Stored gzip, client does not accept it; inflate.
    Decompress,
}

fn choose_representation(
    stored: Compression,
    headers: &HeaderMap,
) -> Result<Representation, ApiError> {
    match stored {
        Compression::None => Ok(Representation::Identity),
        Compression::Gzip | Compression::Brotli | Compression::Zstd => {
            let enc = stored.content_encoding().ok_or(ApiError::Internal)?;
            if accepts_encoding(headers, enc) {
                Ok(Representation::Passthrough(enc))
            } else if stored == Compression::Gzip {
                Ok(Representation::Decompress)
            } else {
                tracing::error!(
                    encoding = enc,
                    "client does not accept the archive's tile encoding and it cannot be decompressed here"
                );
                Err(ApiError::Internal)
            }
        }
        Compression::Unknown => {
            tracing::error!("archive declares unknown tile compression");
            Err(ApiError::Internal)
        }
    }
}

async fn gunzip(raw: Bytes) -> Result<Bytes, ApiError> {
    fn inflate(raw: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(raw.len().saturating_mul(4));
        let mut dec = flate2::read::GzDecoder::new(raw).take(MAX_DECOMPRESSED_TILE + 1);
        dec.read_to_end(&mut out)?;
        if out.len() as u64 > MAX_DECOMPRESSED_TILE {
            return Err(std::io::Error::other("decompressed tile too large"));
        }
        Ok(out)
    }
    let result = if raw.len() >= BLOCKING_DECOMPRESS_THRESHOLD {
        tokio::task::spawn_blocking(move || inflate(&raw))
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "decompression task failed");
                ApiError::Internal
            })?
    } else {
        inflate(&raw)
    };
    result.map(Bytes::from).map_err(|e| {
        tracing::error!(error = %e, "failed to gunzip tile");
        ApiError::Internal
    })
}

/// Static JSON-ish response with the short cache policy.
fn document(content_type: &'static str, body: impl Into<axum::body::Body>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, SHORT_CACHE),
        ],
        body.into(),
    )
        .into_response()
}

/// `GET /metadata` -- raw PMTiles metadata.
pub(crate) async fn get_metadata(State(state): State<std::sync::Arc<AppState>>) -> Response {
    document("application/json", state.archive.metadata.clone())
}

/// Determine the externally visible base URL (no trailing slash).
fn base_url(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Result<String, ApiError> {
    if let Some(url) = &state.public_url {
        return Ok(url.clone());
    }
    let host = match headers.get(header::HOST) {
        Some(v) => v
            .to_str()
            .map_err(|_| ApiError::BadRequest("invalid Host header"))?,
        None => uri
            .authority()
            .map(|a| a.as_str())
            .ok_or(ApiError::BadRequest("missing Host header"))?,
    };
    if !is_valid_authority(host) {
        return Err(ApiError::BadRequest("invalid Host header"));
    }
    // The scheme cannot be learned safely from request headers; deployments
    // behind a TLS terminator should configure `public_url`.
    Ok(format!("http://{host}"))
}

fn with_vary_host(state: &AppState, mut res: Response) -> Response {
    if state.public_url.is_none() {
        res.headers_mut()
            .insert(header::VARY, HeaderValue::from_static("host"));
    }
    res
}

/// `GET /tiles.json` -- TileJSON 3.0.0.
pub(crate) async fn get_tilejson(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, ApiError> {
    let base = base_url(&state, &headers, &uri)?;
    let res = document("application/json", state.archive.tilejson(&base));
    Ok(with_vary_host(&state, res))
}

/// `GET /style.json` -- MapLibre style referencing `/tiles.json`.
pub(crate) async fn get_style(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, ApiError> {
    let base = base_url(&state, &headers, &uri)?;
    let res = document("application/json", state.archive.style(&base));
    Ok(with_vary_host(&state, res))
}

/// `GET /` -- embedded MapLibre viewer.
pub(crate) async fn get_viewer(State(state): State<std::sync::Arc<AppState>>) -> Response {
    let mut res = document(
        "text/html; charset=utf-8",
        state.archive.viewer_html.clone(),
    );
    res.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(VIEWER_CSP),
    );
    res
}

/// `GET /viewer.js`
pub(crate) async fn get_viewer_js() -> Response {
    document(
        "text/javascript; charset=utf-8",
        include_str!("../assets/viewer.js"),
    )
}

/// `GET /viewer.css`
pub(crate) async fn get_viewer_css() -> Response {
    document(
        "text/css; charset=utf-8",
        include_str!("../assets/viewer.css"),
    )
}

/// `GET /healthz` -- liveness.
pub(crate) async fn healthz() -> Response {
    ([(header::CACHE_CONTROL, NO_STORE)], "ok").into_response()
}

/// `GET /readyz` -- readiness. The archive header and metadata were read
/// successfully when the server was opened; readiness fails while draining.
pub(crate) async fn readyz(State(state): State<std::sync::Arc<AppState>>) -> Response {
    let (status, body) = if state.draining.load(Ordering::Acquire) {
        (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
    } else {
        (StatusCode::OK, "ready")
    };
    (status, [(header::CACHE_CONTROL, NO_STORE)], body).into_response()
}
