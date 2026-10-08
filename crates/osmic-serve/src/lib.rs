mod handlers;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::routing::get;
use pmtiles::{AsyncPmTilesReader, MmapBackend};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::info;

use osmic_core::error::{OsmicError, OsmicResult};

/// Shared state for the tile server.
pub struct ServerState {
    pub reader: AsyncPmTilesReader<MmapBackend>,
    pub style_json: String,
    pub pmtiles_path: PathBuf,
}

pub type SharedState = Arc<ServerState>;

/// Configuration for the tile server.
#[derive(Debug, Clone)]
pub struct TileServerConfig {
    pub bind_addr: SocketAddr,
    pub pmtiles_path: PathBuf,
    pub cache_max_age: u32,
}

impl Default for TileServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: ([127, 0, 0, 1], 3000).into(),
            pmtiles_path: PathBuf::from("tiles.pmtiles"),
            cache_max_age: 3600,
        }
    }
}

/// HTTP tile server serving MVT tiles from a PMTiles archive.
pub struct TileServer {
    config: TileServerConfig,
}

impl TileServer {
    pub fn new(config: TileServerConfig) -> Self {
        Self { config }
    }

    /// Start the tile server. Blocks until shutdown.
    pub async fn serve(self) -> OsmicResult<()> {
        let pmtiles_path = &self.config.pmtiles_path;
        if !pmtiles_path.exists() {
            return Err(OsmicError::Other(format!(
                "PMTiles file not found: {}",
                pmtiles_path.display()
            )));
        }

        info!(path = %pmtiles_path.display(), "Opening PMTiles archive");
        let backend = MmapBackend::try_from(pmtiles_path.as_path())
            .await
            .map_err(|e| OsmicError::Other(format!("Failed to open PMTiles: {e}")))?;
        let reader: AsyncPmTilesReader<MmapBackend> = AsyncPmTilesReader::try_from_source(backend)
            .await
            .map_err(|e| OsmicError::Other(format!("Failed to read PMTiles: {e}")))?;

        let tile_url = format!("http://{}/tiles/{{z}}/{{x}}/{{y}}", self.config.bind_addr);
        let style = osmic_style::default_style_json(&tile_url);
        let style_json = style.to_json();

        let state: SharedState = Arc::new(ServerState {
            reader,
            style_json,
            pmtiles_path: pmtiles_path.clone(),
        });

        let cache_value = format!("public, max-age={}", self.config.cache_max_age);

        let app = Router::new()
            .route("/tiles/{z}/{x}/{y}", get(handlers::get_tile))
            .route("/metadata", get(handlers::get_metadata))
            .route("/style.json", get(handlers::get_style))
            .route("/", get(handlers::get_viewer))
            .layer(CompressionLayer::new())
            .layer(CorsLayer::permissive())
            .layer(SetResponseHeaderLayer::if_not_present(
                header::CACHE_CONTROL,
                HeaderValue::from_str(&cache_value)
                    .unwrap_or_else(|_| HeaderValue::from_static("public, max-age=3600")),
            ))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(self.config.bind_addr)
            .await
            .map_err(|e| OsmicError::Other(format!("Failed to bind: {e}")))?;

        info!(addr = %self.config.bind_addr, "Tile server listening");
        println!("Tile server: http://{}", self.config.bind_addr);
        println!("  Viewer:    http://{}/", self.config.bind_addr);
        println!(
            "  Tiles:     http://{}/tiles/<z>/<x>/<y>",
            self.config.bind_addr
        );
        println!("  Style:     http://{}/style.json", self.config.bind_addr);
        println!("  Metadata:  http://{}/metadata", self.config.bind_addr);

        axum::serve(listener, app)
            .await
            .map_err(|e| OsmicError::Other(format!("Server error: {e}")))?;

        Ok(())
    }
}

/// Plugin that adds a tile server to the App.
pub struct TileServerPlugin {
    pub config: TileServerConfig,
}

impl TileServerPlugin {
    pub fn new(pmtiles_path: impl AsRef<Path>) -> Self {
        Self {
            config: TileServerConfig {
                pmtiles_path: pmtiles_path.as_ref().to_path_buf(),
                ..Default::default()
            },
        }
    }

    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.config.bind_addr = addr;
        self
    }
}

impl osmic_app::Plugin for TileServerPlugin {
    fn build(&self, app: &mut osmic_app::App) {
        app.insert_resource(self.config.clone());
    }

    fn name(&self) -> &str {
        "TileServerPlugin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_binds_to_loopback() {
        let config = TileServerConfig::default();
        assert!(config.bind_addr.ip().is_loopback());
        assert_eq!(config.bind_addr.port(), 3000);
        assert_eq!(config.cache_max_age, 3600);
    }

    #[test]
    fn tile_server_new_preserves_config() {
        let config = TileServerConfig {
            bind_addr: ([127, 0, 0, 1], 4444).into(),
            pmtiles_path: PathBuf::from("/tmp/example.pmtiles"),
            cache_max_age: 60,
        };
        let server = TileServer::new(config.clone());
        assert_eq!(server.config.bind_addr, config.bind_addr);
        assert_eq!(server.config.cache_max_age, 60);
    }

    #[tokio::test]
    async fn serve_errors_when_pmtiles_missing() {
        let config = TileServerConfig {
            bind_addr: ([127, 0, 0, 1], 0).into(),
            pmtiles_path: PathBuf::from("/nonexistent/definitely-missing.pmtiles"),
            cache_max_age: 60,
        };
        let server = TileServer::new(config);
        let err = server.serve().await.expect_err("should fail without file");
        match err {
            OsmicError::Other(msg) => assert!(msg.contains("not found")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
