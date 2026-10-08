//! Router assembly and server lifecycle.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::load_shed::error::Overloaded;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::archive::Archive;
use crate::config::{TileServerConfig, ValidatedConfig};
use crate::error::ServeError;
use crate::handlers::{self, AppState};
use crate::http::NO_STORE;
use crate::routes::ServerRoutes;

/// HTTP tile server for a single PMTiles archive.
///
/// Create it with [`TileServer::open`], then either run it with
/// [`serve`](Self::serve) / [`serve_with_shutdown`](Self::serve_with_shutdown)
/// or embed [`router`](Self::router) in a larger application (or drive it
/// with `tower::ServiceExt::oneshot` in tests).
pub struct TileServer {
    config: TileServerConfig,
    validated: ValidatedConfig,
    state: Arc<AppState>,
    /// Routes before the middleware layers, for [`TileServer::with_routes`].
    routes: Router,
    router: Router,
}

impl std::fmt::Debug for TileServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileServer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TileServer {
    /// Validate `config`, open and memory-map the archive and build the router.
    ///
    /// # Errors
    ///
    /// [`ServeError::InvalidConfig`] for bad settings,
    /// [`ServeError::ArchiveNotFound`] / [`ServeError::Archive`] if the
    /// archive is missing or unreadable.
    pub async fn open(config: TileServerConfig) -> Result<Self, ServeError> {
        let validated = config.validate()?;
        let archive = Archive::open(&config.pmtiles_path).await?;
        let cache = format!("public, max-age={}", config.cache_max_age);
        let tile_cache_control =
            HeaderValue::from_str(&cache).map_err(|e| ServeError::InvalidConfig {
                field: "cache_max_age",
                reason: e.to_string(),
            })?;
        let state = Arc::new(AppState {
            archive,
            tile_cache_control,
            public_url: validated.public_url.clone(),
            draining: AtomicBool::new(false),
        });
        let routes = build_routes(Arc::clone(&state));
        let router = apply_resource_controls(routes.clone(), &config, &validated);
        Ok(Self {
            config,
            validated,
            state,
            routes,
            router,
        })
    }

    /// Serve `extra` routes next to the built-in endpoints, behind the same
    /// middleware.
    ///
    /// # Errors
    ///
    /// [`ServeError::InvalidConfig`] for an invalid or overlapping prefix
    /// (see [`ServerRoutes::nest`]).
    pub fn with_routes(mut self, extra: ServerRoutes) -> Result<Self, ServeError> {
        if extra.is_empty() {
            return Ok(self);
        }
        self.routes = extra.mount(self.routes)?;
        self.router = apply_resource_controls(self.routes.clone(), &self.config, &self.validated);
        Ok(self)
    }

    /// The configuration this server was opened with.
    pub fn config(&self) -> &TileServerConfig {
        &self.config
    }

    /// The fully layered [`Router`] (timeout, load shedding, CORS, tracing).
    ///
    /// Clones share one concurrency budget. The router holds the open
    /// archive, so it stays valid after the `TileServer` is dropped.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Bind [`TileServerConfig::bind_addr`] and serve until SIGINT or SIGTERM,
    /// then drain in-flight requests.
    ///
    /// # Errors
    ///
    /// [`ServeError::Bind`] if the address cannot be bound, otherwise as
    /// [`serve_with_shutdown`](Self::serve_with_shutdown).
    pub async fn serve(self) -> Result<(), ServeError> {
        let addr = self.config.bind_addr;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServeError::Bind { addr, source })?;
        self.serve_with_shutdown(listener, shutdown_signal()).await
    }

    /// Serve on an existing listener until `shutdown` completes, then stop
    /// accepting connections and wait for in-flight requests to finish.
    ///
    /// `/readyz` starts failing as soon as `shutdown` completes so load
    /// balancers can drain the instance.
    ///
    /// # Errors
    ///
    /// [`ServeError::Serve`] if the HTTP server fails.
    pub async fn serve_with_shutdown<F>(
        self,
        listener: TcpListener,
        shutdown: F,
    ) -> Result<(), ServeError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match listener.local_addr() {
            Ok(addr) => tracing::info!(%addr, "tile server listening"),
            Err(e) => tracing::warn!(error = %e, "tile server listening (address unknown)"),
        }
        let state = self.state;
        axum::serve(listener, self.router)
            .with_graceful_shutdown(async move {
                shutdown.await;
                state.draining.store(true, Ordering::Release);
                tracing::info!("shutdown requested; draining in-flight requests");
            })
            .await
            .map_err(ServeError::Serve)?;
        tracing::info!("tile server stopped");
        Ok(())
    }
}

/// Resolves on SIGINT (Ctrl-C) or, on Unix, SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to install Ctrl-C handler");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

fn build_routes(state: Arc<AppState>) -> Router {
    // Tiles are served pre-compressed (or deliberately identity); never
    // recompress them. Only the small JSON/HTML documents go through gzip.
    let tiles = Router::new().route("/tiles/{z}/{x}/{y}", get(handlers::get_tile));
    let documents = Router::new()
        .route("/", get(handlers::get_viewer))
        .route("/viewer.js", get(handlers::get_viewer_js))
        .route("/viewer.css", get(handlers::get_viewer_css))
        .route("/metadata", get(handlers::get_metadata))
        .route("/tiles.json", get(handlers::get_tilejson))
        .route("/style.json", get(handlers::get_style))
        .layer(CompressionLayer::new());
    let probes = Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz));

    Router::new()
        .merge(tiles)
        .merge(documents)
        .merge(probes)
        .with_state(state)
}

/// Wrap `router` with the cross-cutting middleware. Outermost first:
/// tracing, error cache hygiene, CORS, load shedding, timeout.
fn apply_resource_controls(
    router: Router,
    config: &TileServerConfig,
    v: &ValidatedConfig,
) -> Router {
    let allow_origin = if v.cors_origins.is_empty() {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(v.cors_origins.clone())
    };
    let cors = CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([Method::GET, Method::HEAD, Method::OPTIONS])
        .allow_headers([header::IF_NONE_MATCH, header::ACCEPT])
        .expose_headers([
            header::ETAG,
            header::CONTENT_ENCODING,
            header::CONTENT_LENGTH,
        ])
        .max_age(std::time::Duration::from_secs(86_400));

    let trace = TraceLayer::new_for_http()
        .make_span_with(DefaultMakeSpan::new().level(Level::DEBUG))
        .on_response(DefaultOnResponse::new().level(Level::DEBUG));

    router.layer(
        ServiceBuilder::new()
            .layer(trace)
            .layer(map_response(no_store_on_error))
            .layer(SetResponseHeaderLayer::if_not_present(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ))
            .layer(cors)
            .layer(HandleErrorLayer::new(handle_overload))
            .layer(LoadShedLayer::new())
            .layer(GlobalConcurrencyLimitLayer::new(config.max_concurrency))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                config.request_timeout,
            )),
    )
}

/// Safety net: whatever produced an error (handler, extractor, router,
/// timeout, load shedder), it must not be cached.
async fn no_store_on_error(mut res: Response) -> Response {
    if res.status().is_client_error() || res.status().is_server_error() {
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    }
    res
}

async fn handle_overload(err: tower::BoxError) -> Response {
    if err.is::<Overloaded>() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            "server busy",
        )
            .into_response()
    } else {
        tracing::error!(error = %err, "unhandled middleware error");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn slow() -> &'static str {
        tokio::time::sleep(Duration::from_millis(300)).await;
        "done"
    }

    fn controlled(config: &TileServerConfig) -> Router {
        let v = config.validate().unwrap();
        apply_resource_controls(Router::new().route("/slow", get(slow)), config, &v)
    }

    #[tokio::test]
    async fn sheds_load_when_saturated() {
        let config = TileServerConfig::new("x").max_concurrency(1);
        let app = controlled(&config);
        let req = || Request::get("/slow").body(Body::empty()).unwrap();
        let first = tokio::spawn(app.clone().oneshot(req()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let shed = app.clone().oneshot(req()).await.unwrap();
        assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(shed.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn times_out_slow_requests() {
        let config = TileServerConfig::new("x").request_timeout(Duration::from_millis(50));
        let res = controlled(&config)
            .oneshot(Request::get("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    }
}
