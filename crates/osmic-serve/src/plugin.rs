//! `osmic-app` integration.

use std::net::SocketAddr;
use std::path::Path;

use crate::config::TileServerConfig;

/// Plugin that registers a [`TileServerConfig`] resource on the `App`.
///
/// It only stores the configuration; starting the server is up to the host.
#[derive(Debug, Clone)]
pub struct TileServerPlugin {
    /// Configuration inserted into the app.
    pub config: TileServerConfig,
}

impl TileServerPlugin {
    /// Create a plugin serving `pmtiles_path` with default settings.
    pub fn new(pmtiles_path: impl AsRef<Path>) -> Self {
        Self {
            config: TileServerConfig::new(pmtiles_path.as_ref()),
        }
    }

    /// Set the listen address.
    #[must_use]
    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.config = self.config.bind_addr(addr);
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
