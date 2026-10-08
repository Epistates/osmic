use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse};

use crate::SharedState;

/// Serve a vector tile from the PMTiles archive.
pub async fn get_tile(
    State(state): State<SharedState>,
    Path((z, x, y)): Path<(u8, u32, u32)>,
) -> impl IntoResponse {
    let coord = match pmtiles::TileCoord::new(z, x, y) {
        Ok(c) => c,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid tile coordinates").into_response(),
    };

    match state.reader.get_tile_decompressed(coord).await {
        Ok(Some(data)) => {
            tracing::info!(z, x, y, bytes = data.len(), "tile hit");
            let h = state.reader.get_header();
            // Serve decompressed MVT; let tower-http CompressionLayer handle encoding
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, h.tile_type.content_type())],
                data,
            )
                .into_response()
        }
        Ok(None) => {
            tracing::info!(z, x, y, "tile miss (no data)");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!(z, x, y, error = %e, "Tile read error");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

/// Serve PMTiles metadata as JSON.
pub async fn get_metadata(State(state): State<SharedState>) -> impl IntoResponse {
    match state.reader.get_metadata().await {
        Ok(json) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Metadata read error");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

/// Serve the MapLibre-compatible style JSON (no-cache to avoid stale URLs).
pub async fn get_style(State(state): State<SharedState>) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    (StatusCode::OK, headers, state.style_json.clone())
}

/// Serve an embedded MapLibre GL JS viewer page.
///
/// The viewer auto-fits to the PMTiles bounding box and opens at the
/// file's max zoom, so POI-only tile sets (which have minzoom ≥ 13)
/// are immediately populated instead of showing blank tiles at z=0.
pub async fn get_viewer(State(state): State<SharedState>) -> impl IntoResponse {
    let h = state.reader.get_header();

    // Center of the PMTiles bounding box. Fall back to the continental
    // US centroid if the bbox is degenerate (e.g. an empty archive).
    let (min_lon, min_lat, max_lon, max_lat) = (
        h.min_longitude,
        h.min_latitude,
        h.max_longitude,
        h.max_latitude,
    );
    let (center_lon, center_lat) = if max_lon > min_lon && max_lat > min_lat {
        ((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0)
    } else {
        (-98.5, 39.8)
    };

    // Open at max_zoom - 1 so the user sees a couple of tiles worth of
    // context around the center. Clamp to the archive's supported range.
    let open_zoom = h.max_zoom.saturating_sub(1).max(h.min_zoom);

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Osmic Viewer</title>
    <script src="https://unpkg.com/maplibre-gl@5.3.0/dist/maplibre-gl.js"></script>
    <link href="https://unpkg.com/maplibre-gl@5.3.0/dist/maplibre-gl.css" rel="stylesheet">
    <style>
        body {{ margin: 0; font-family: -apple-system, system-ui, sans-serif; }}
        #map {{ width: 100%; height: 100vh; }}
        .info {{
            position: absolute; top: 10px; left: 10px; z-index: 1;
            background: rgba(255,255,255,0.92); padding: 8px 12px;
            border-radius: 4px; font-size: 13px;
            box-shadow: 0 1px 3px rgba(0,0,0,0.2);
        }}
        .info a {{ color: #0366d6; text-decoration: none; margin: 0 4px; }}
        .info a:hover {{ text-decoration: underline; }}
        .coord {{
            position: absolute; bottom: 10px; left: 10px; z-index: 1;
            background: rgba(255,255,255,0.92); padding: 4px 8px;
            border-radius: 3px; font-size: 12px; font-family: monospace;
        }}
    </style>
</head>
<body>
    <div id="map"></div>
    <div class="info">
        <strong>Osmic</strong>
        <a href="/style.json">style</a> ·
        <a href="/metadata">metadata</a> ·
        <a href="/tiles/{open_zoom}/0/0">tile0</a>
    </div>
    <div class="coord" id="coord">zoom {open_zoom} · center {center_lon:.4},{center_lat:.4}</div>
    <script>
        const map = new maplibregl.Map({{
            container: 'map',
            style: '/style.json?v=' + Date.now(),
            center: [{center_lon}, {center_lat}],
            zoom: {open_zoom},
            minZoom: {min_zoom},
            maxZoom: {max_zoom},
        }});
        map.addControl(new maplibregl.NavigationControl());
        map.addControl(new maplibregl.ScaleControl());
        // Fit to the PMTiles bbox on initial load if it's meaningful.
        map.once('load', () => {{
            const bbox = [[{min_lon}, {min_lat}], [{max_lon}, {max_lat}]];
            if ({min_lon} !== {max_lon} && {min_lat} !== {max_lat}) {{
                map.fitBounds(bbox, {{ padding: 40, maxZoom: {max_zoom} }});
            }}
        }});
        // Live coordinate readout.
        const coord = document.getElementById('coord');
        map.on('move', () => {{
            const c = map.getCenter();
            coord.textContent = `zoom ${{map.getZoom().toFixed(1)}} · center ${{c.lng.toFixed(4)}},${{c.lat.toFixed(4)}}`;
        }});
    </script>
</body>
</html>"#,
        min_zoom = h.min_zoom,
        max_zoom = h.max_zoom,
        open_zoom = open_zoom,
        center_lon = center_lon,
        center_lat = center_lat,
        min_lon = min_lon,
        min_lat = min_lat,
        max_lon = max_lon,
        max_lat = max_lat,
    );

    Html(html)
}
