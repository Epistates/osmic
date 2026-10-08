//! The opened PMTiles archive plus everything derived from it that is
//! immutable for the lifetime of the server (metadata, TileJSON and style
//! templates, viewer page).

use std::path::Path;

use bytes::Bytes;
use pmtiles::{AsyncPmTilesReader, Compression, Header, MmapBackend, TileType};
use serde_json::{Map, Value, json};

use crate::error::ServeError;

/// Placeholder replaced by the request's base URL in the pre-rendered
/// TileJSON and style documents. The substituted value is always validated
/// (see [`crate::http::is_valid_authority`]) so it is safe inside JSON strings.
const BASE_TOKEN: &str = "@@OSMIC_BASE@@";

/// Highest zoom MapLibre GL accepts.
const MAPLIBRE_MAX_ZOOM: u8 = 24;

pub(crate) struct Archive {
    reader: AsyncPmTilesReader<MmapBackend>,
    pub tile_type: TileType,
    pub tile_compression: Compression,
    pub max_zoom: u8,
    pub metadata: Bytes,
    tilejson_template: String,
    style_template: String,
    pub viewer_html: String,
}

impl Archive {
    pub async fn open(path: &Path) -> Result<Self, ServeError> {
        let archive_err = |source| ServeError::Archive {
            path: path.to_path_buf(),
            source,
        };
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Err(ServeError::ArchiveNotFound(path.to_path_buf()));
        }
        tracing::info!(path = %path.display(), "opening PMTiles archive");
        let backend = MmapBackend::try_from(path).await.map_err(archive_err)?;
        let reader = AsyncPmTilesReader::try_from_source(backend)
            .await
            .map_err(archive_err)?;
        // Reading the metadata up front proves the header and the metadata
        // block are both readable, so `/readyz` can be a pure flag check.
        let metadata = reader.get_metadata().await.map_err(archive_err)?;

        let header = reader.get_header();
        let (tile_type, tile_compression, max_zoom) =
            (header.tile_type, header.tile_compression, header.max_zoom);
        let meta_json = serde_json::from_str::<Value>(&metadata).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "archive metadata is not valid JSON; TileJSON will be minimal");
            Value::Null
        });
        let tilejson_template = build_tilejson(header, &meta_json).to_string();
        let viewer_html = build_viewer_html(header);

        let mut style = osmic_style::default_style_json(BASE_TOKEN);
        style.sources = json!({
            "osmic": { "type": "vector", "url": format!("{BASE_TOKEN}/tiles.json") }
        });
        let style_template = style.to_json();

        Ok(Self {
            reader,
            tile_type,
            tile_compression,
            max_zoom,
            metadata: Bytes::from(metadata),
            tilejson_template,
            style_template,
            viewer_html,
        })
    }

    pub fn reader(&self) -> &AsyncPmTilesReader<MmapBackend> {
        &self.reader
    }

    /// TileJSON 3.0.0 document with absolute tile URLs rooted at `base`.
    pub fn tilejson(&self, base: &str) -> String {
        self.tilejson_template.replace(BASE_TOKEN, base)
    }

    /// MapLibre style whose vector source points at `{base}/tiles.json`.
    pub fn style(&self, base: &str) -> String {
        self.style_template.replace(BASE_TOKEN, base)
    }
}

/// File extension advertised in TileJSON tile URLs.
pub(crate) fn tile_extension(t: TileType) -> &'static str {
    match t {
        TileType::Mvt => "mvt",
        TileType::Mlt => "mlt",
        TileType::Png => "png",
        TileType::Jpeg => "jpg",
        TileType::Webp => "webp",
        TileType::Avif => "avif",
        TileType::Unknown => "bin",
    }
}

fn finite_in(v: f64, lo: f64, hi: f64) -> bool {
    v.is_finite() && (lo..=hi).contains(&v)
}

/// `[west, south, east, north]` if the header bounds are finite, in range and non-degenerate.
fn sane_bounds(h: &Header) -> Option<[f64; 4]> {
    let ok = finite_in(h.min_longitude, -180.0, 180.0)
        && finite_in(h.max_longitude, -180.0, 180.0)
        && finite_in(h.min_latitude, -90.0, 90.0)
        && finite_in(h.max_latitude, -90.0, 90.0)
        && h.max_longitude > h.min_longitude
        && h.max_latitude > h.min_latitude;
    ok.then_some([
        h.min_longitude,
        h.min_latitude,
        h.max_longitude,
        h.max_latitude,
    ])
}

fn sane_center(h: &Header) -> Option<[f64; 2]> {
    (finite_in(h.center_longitude, -180.0, 180.0) && finite_in(h.center_latitude, -90.0, 90.0))
        .then_some([h.center_longitude, h.center_latitude])
}

fn build_tilejson(h: &Header, meta: &Value) -> Value {
    let ext = tile_extension(h.tile_type);
    let mut doc = Map::new();
    doc.insert("tilejson".into(), "3.0.0".into());
    doc.insert("scheme".into(), "xyz".into());
    doc.insert(
        "tiles".into(),
        json!([format!("{BASE_TOKEN}/tiles/{{z}}/{{x}}/{{y}}.{ext}")]),
    );
    doc.insert("minzoom".into(), h.min_zoom.into());
    doc.insert("maxzoom".into(), h.max_zoom.into());
    if let Some(b) = sane_bounds(h) {
        doc.insert("bounds".into(), json!(b));
    }
    if let Some([lon, lat]) = sane_center(h) {
        let zoom = h.center_zoom.clamp(h.min_zoom, h.max_zoom);
        doc.insert("center".into(), json!([lon, lat, zoom]));
    }
    if let Value::Object(m) = meta {
        for key in ["name", "description", "attribution", "version", "legend"] {
            if let Some(v @ Value::String(_)) = m.get(key) {
                doc.insert(key.into(), v.clone());
            }
        }
    }
    if matches!(h.tile_type, TileType::Mvt | TileType::Mlt) {
        doc.insert("vector_layers".into(), vector_layers(meta));
    }
    Value::Object(doc)
}

/// Find `vector_layers` either at the top level of the metadata or inside the
/// tippecanoe-style `json` string member.
fn vector_layers(meta: &Value) -> Value {
    if let Some(v @ Value::Array(_)) = meta.get("vector_layers") {
        return v.clone();
    }
    meta.get("json")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|inner| inner.get("vector_layers").filter(|v| v.is_array()).cloned())
        .unwrap_or_else(|| json!([]))
}

fn escape_html_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Viewer page. Archive-derived values are passed to the (static) script as a
/// JSON document in a `data-config` attribute, never interpolated into code.
fn build_viewer_html(h: &Header) -> String {
    let bounds = sane_bounds(h);
    let (center, zoom) = match (bounds, sane_center(h)) {
        // Open one level below max zoom so a few tiles of context show up even
        // for POI-only archives whose minzoom is high.
        (Some(b), _) => (
            [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0],
            h.max_zoom.saturating_sub(1).max(h.min_zoom),
        ),
        (None, Some(c)) => (c, h.center_zoom.clamp(h.min_zoom, h.max_zoom)),
        (None, None) => ([0.0, 0.0], 0),
    };
    let config = json!({
        "center": center,
        "zoom": zoom.min(MAPLIBRE_MAX_ZOOM),
        "minZoom": h.min_zoom.min(MAPLIBRE_MAX_ZOOM),
        "maxZoom": h.max_zoom.min(MAPLIBRE_MAX_ZOOM),
        "bounds": bounds,
    });
    let config = escape_html_attr(&config.to_string());
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Osmic Viewer</title>
<link href="{css_url}" rel="stylesheet" integrity="{css_sri}" crossorigin="anonymous">
<link href="/viewer.css" rel="stylesheet">
</head>
<body>
<div id="map" data-config="{config}"></div>
<div class="info">
<strong>Osmic</strong>
<a href="/style.json">style</a> &middot;
<a href="/tiles.json">tilejson</a> &middot;
<a href="/metadata">metadata</a>
</div>
<div class="coord" id="coord"></div>
<script src="{js_url}" integrity="{js_sri}" crossorigin="anonymous"></script>
<script src="/viewer.js"></script>
</body>
</html>
"#,
        css_url = MAPLIBRE_CSS_URL,
        css_sri = MAPLIBRE_CSS_SRI,
        js_url = MAPLIBRE_JS_URL,
        js_sri = MAPLIBRE_JS_SRI,
    )
}

// maplibre-gl 5.3.0, hashes computed from the exact files served by unpkg.
const MAPLIBRE_JS_URL: &str = "https://unpkg.com/maplibre-gl@5.3.0/dist/maplibre-gl.js";
const MAPLIBRE_JS_SRI: &str =
    "sha384-lwoweBvLd/SJV/pFn4C9Dj9MR+0hltVUArfmLy5PPNG2vOnTCmy+w+gxbO/wzFgH";
const MAPLIBRE_CSS_URL: &str = "https://unpkg.com/maplibre-gl@5.3.0/dist/maplibre-gl.css";
const MAPLIBRE_CSS_SRI: &str =
    "sha384-Nq6PQ+9vJPvw7U/VfDELyrWoGQMsy0gi6QShhaSrGzkpF5KkM40csg2leky+YMTd";

/// Content-Security-Policy for the viewer page.
pub(crate) const VIEWER_CSP: &str = "default-src 'none'; \
script-src 'self' https://unpkg.com; \
style-src 'self' https://unpkg.com; \
style-src-attr 'unsafe-inline'; \
img-src 'self' data: blob:; \
connect-src 'self' https://fonts.openmaptiles.org; \
worker-src blob:; child-src blob:; \
base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
