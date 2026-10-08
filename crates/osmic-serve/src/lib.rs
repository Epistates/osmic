//! HTTP tile server for [PMTiles](https://github.com/protomaps/PMTiles) archives.
//!
//! ```no_run
//! # async fn run() -> Result<(), osmic_serve::ServeError> {
//! use osmic_serve::{TileServer, TileServerConfig};
//!
//! let config = TileServerConfig::new("tiles.pmtiles")
//!     .bind_addr(([127, 0, 0, 1], 3000).into())
//!     .public_url("https://tiles.example.com");
//! // Serves until SIGINT/SIGTERM, then drains in-flight requests.
//! TileServer::open(config).await?.serve().await
//! # }
//! ```
//!
//! # Endpoints
//!
//! | Path | Description |
//! |------|-------------|
//! | `/tiles/{z}/{x}/{y}` (also `.mvt`, `.pbf`, `.mlt`) | A tile; `204` if absent, `404` above the archive's max zoom, `400` for invalid coordinates |
//! | `/tiles.json` | TileJSON 3.0.0 |
//! | `/style.json` | MapLibre style whose source points at `/tiles.json` |
//! | `/metadata` | Raw PMTiles metadata |
//! | `/` | Embedded MapLibre GL viewer |
//! | `/healthz`, `/readyz` | Liveness and readiness probes |
//!
//! Applications can serve their own routes under other prefixes, behind the
//! same middleware, with [`TileServer::with_routes`] (or the [`ServerRoutes`]
//! resource when using [`TileServerPlugin`]).
//!
//! # Tile delivery
//!
//! Tiles are served byte-for-byte from the archive. When the archive stores
//! gzip (or brotli / zstd) tiles and the client's `Accept-Encoding` allows that
//! coding, the stored bytes are sent with `Content-Encoding`; gzip tiles are
//! inflated for clients that do not accept gzip. Responses always carry
//! `Vary: Accept-Encoding` and a strong `ETag` (xxh3-64 of the stored bytes,
//! stable across restarts); `If-None-Match` yields `304`. The `Content-Type`
//! follows the archive's tile type (MVT, MLT or raster).
//!
//! # Caching
//!
//! Tile responses, including `204` for absent tiles (the archive is immutable
//! while served), use `public, max-age=<cache_max_age>`. Documents use a
//! 60 second `max-age`. Every `4xx`/`5xx` response is `no-store`.
//!
//! # Resource controls
//!
//! A per-request timeout (`408`), a concurrency limit with load shedding
//! (`503` + `Retry-After`), graceful shutdown on SIGINT/SIGTERM and
//! restrictive CORS (GET/HEAD/OPTIONS, any origin unless configured, no
//! credentials) are built in. Per-request logs are emitted at `debug` level.
//!
//! # Memory-mapped archive
//!
//! The archive is memory-mapped. **Replace it atomically**: write the new file
//! next to the old one and `rename(2)` it over the original, then restart (or
//! roll) the server. Truncating or rewriting the file in place while it is
//! served can crash the process (`SIGBUS`) or serve corrupt tiles, and is
//! unsupported.
//!
//! # URLs behind a proxy
//!
//! Absolute URLs in `/style.json` and `/tiles.json` come from
//! [`TileServerConfig::public_url`] when set. Otherwise they are built from the
//! strictly validated `Host` header with the `http` scheme (invalid hosts
//! get `400`); set `public_url` when TLS is terminated in front of the server.

#![warn(missing_docs)]

mod archive;
mod config;
mod error;
mod handlers;
mod http;
mod plugin;
mod routes;
mod server;

pub use config::TileServerConfig;
pub use error::ServeError;
pub use plugin::TileServerPlugin;
pub use routes::ServerRoutes;
pub use server::{TileServer, shutdown_signal};
