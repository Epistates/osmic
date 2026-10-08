//! `osmic-app` integration.

use std::net::SocketAddr;
use std::path::Path;

use osmic_app::{App, BoxError, Plugin};

use crate::config::TileServerConfig;
use crate::routes::ServerRoutes;
use crate::server::TileServer;

/// Serves a PMTiles archive as the app's runner.
///
/// During build the plugin validates its configuration (so mistakes fail
/// before anything starts), inserts it as a [`TileServerConfig`] resource
/// that later plugins may adjust, provides an empty [`ServerRoutes`]
/// resource for other plugins' routes, and installs a runner that opens
/// the archive and serves until SIGINT/SIGTERM on its own Tokio runtime.
///
/// ```no_run
/// use osmic_app::App;
/// use osmic_serve::TileServerPlugin;
///
/// App::new()
///     .add_plugin(TileServerPlugin::new("tiles.pmtiles").with_addr(([0, 0, 0, 0], 8080).into()))
///     .run()?;
/// # Ok::<(), osmic_app::AppError>(())
/// ```
///
/// The runner refuses to start inside an existing Tokio runtime; async
/// applications should use [`TileServer`] directly.
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

    /// Use `config` instead of the defaults.
    pub fn with_config(config: TileServerConfig) -> Self {
        Self { config }
    }

    /// Set the listen address.
    #[must_use]
    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.config = self.config.bind_addr(addr);
        self
    }
}

impl Plugin for TileServerPlugin {
    fn build(&self, app: &mut App) -> Result<(), BoxError> {
        self.config.validate()?;
        app.insert_resource(self.config.clone());
        if !app.contains_resource::<ServerRoutes>() {
            app.insert_resource(ServerRoutes::default());
        }
        app.set_runner(serve_app);
        Ok(())
    }

    fn name(&self) -> &str {
        "TileServerPlugin"
    }
}

fn serve_app(app: &mut App) -> Result<(), BoxError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(
            "TileServerPlugin cannot run inside a Tokio runtime; use TileServer directly".into(),
        );
    }
    let config = app.resource::<TileServerConfig>()?.clone();
    let routes = app.remove_resource::<ServerRoutes>().unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("osmic-serve")
        .build()?;
    runtime.block_on(async {
        TileServer::open(config)
            .await?
            .with_routes(routes)?
            .serve()
            .await
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use osmic_app::AppError;

    #[test]
    fn invalid_config_fails_the_build() {
        let mut app = App::new();
        app.add_plugin(TileServerPlugin::with_config(
            TileServerConfig::new("x.pmtiles").max_concurrency(0),
        ));
        let err = app.build().expect_err("invalid");
        assert!(err.to_string().contains("max_concurrency"), "{err}");
    }

    #[test]
    fn provides_config_and_routes() {
        let mut app = App::new();
        app.add_plugin(TileServerPlugin::new("x.pmtiles"));
        app.build().expect("builds");
        assert_eq!(
            app.resource::<TileServerConfig>()
                .expect("config")
                .pmtiles_path,
            Path::new("x.pmtiles")
        );
        assert!(app.contains_resource::<ServerRoutes>());
    }

    #[test]
    fn missing_archive_is_a_runner_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::new();
        app.add_plugin(
            TileServerPlugin::new(dir.path().join("absent.pmtiles"))
                .with_addr(([127, 0, 0, 1], 0).into()),
        );
        let err = app.run().expect_err("no archive");
        assert!(matches!(err, AppError::Runner(_)));
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn refuses_to_nest_runtimes() {
        let mut app = App::new();
        app.add_plugin(TileServerPlugin::new("x.pmtiles"));
        let err = app.run().expect_err("inside a runtime");
        assert!(err.to_string().contains("Tokio runtime"), "{err}");
    }
}
