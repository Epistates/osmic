//! `osmic serve`

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Args;

use osmic_serve::{TileServer, TileServerConfig};

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// PMTiles archive to serve
    pub pmtiles: PathBuf,

    /// Address to bind
    #[arg(long, default_value = "127.0.0.1:3000")]
    pub bind: SocketAddr,

    /// Cache-Control max-age for tile responses, in seconds
    #[arg(long, default_value_t = 3600)]
    pub cache_max_age: u32,

    /// Public base URL used in TileJSON and style.json (required behind a
    /// TLS-terminating proxy); default: derived from the Host header
    #[arg(long)]
    pub public_url: Option<String>,

    /// Allowed CORS origin (repeatable); default: any origin
    #[arg(long = "cors-origin")]
    pub cors_origins: Vec<String>,

    /// Per-request timeout in seconds
    #[arg(long, default_value_t = 30)]
    pub request_timeout: u64,

    /// Requests handled concurrently before shedding load with 503
    #[arg(long, default_value_t = 1024)]
    pub max_concurrency: usize,

    /// Open client connections accepted at once
    #[arg(long, default_value_t = 4096)]
    pub max_connections: usize,

    /// Seconds a client has to send request headers (also the idle
    /// keep-alive limit)
    #[arg(long, default_value_t = 10)]
    pub header_timeout: u64,

    /// After SIGINT/SIGTERM, keep serving this many seconds with /readyz
    /// failing so load balancers stop routing here (use 5-10 behind
    /// Kubernetes)
    #[arg(long, default_value_t = 0)]
    pub drain_delay: u64,

    /// Longest wait in seconds for in-flight requests during shutdown
    #[arg(long, default_value_t = 30)]
    pub drain_timeout: u64,

    /// Decoded PMTiles leaf directories kept in memory
    #[arg(long, default_value_t = osmic_tiles::reader::DEFAULT_DIRECTORY_CACHE)]
    pub directory_cache: u64,
}

pub fn run(args: ServeArgs) -> anyhow::Result<()> {
    if !args.pmtiles.is_file() {
        bail!("{} does not exist", args.pmtiles.display());
    }
    let mut config = TileServerConfig::new(&args.pmtiles)
        .bind_addr(args.bind)
        .cache_max_age(args.cache_max_age)
        .cors_allowed_origins(args.cors_origins.clone())
        .request_timeout(Duration::from_secs(args.request_timeout))
        .max_concurrency(args.max_concurrency)
        .max_connections(args.max_connections)
        .header_read_timeout(Duration::from_secs(args.header_timeout))
        .drain_delay(Duration::from_secs(args.drain_delay))
        .drain_timeout(Duration::from_secs(args.drain_timeout))
        .directory_cache(args.directory_cache);
    if let Some(url) = &args.public_url {
        config = config.public_url(url);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(async {
            let server = TileServer::open(config).await?;
            eprintln!(
                "Serving {} at http://{}/",
                args.pmtiles.display(),
                args.bind
            );
            server.serve().await
        })
        .with_context(|| format!("serving {}", args.pmtiles.display()))
}
