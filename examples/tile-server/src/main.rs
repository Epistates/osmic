use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use osmic_serve::{TileServer, TileServerConfig};

#[derive(Parser)]
#[command(
    name = "tile-server",
    about = "Serve vector tiles from a PMTiles archive"
)]
struct Args {
    /// Path to the PMTiles file
    pmtiles_file: PathBuf,

    /// Address to bind to
    #[arg(long, default_value = "127.0.0.1:3000")]
    bind: SocketAddr,

    /// Cache-Control max-age in seconds
    #[arg(long, default_value = "3600")]
    cache_max_age: u32,

    /// Externally visible base URL (e.g. `https://tiles.example.com`)
    #[arg(long)]
    public_url: Option<String>,

    /// Allowed CORS origin (repeatable); any origin if omitted
    #[arg(long = "cors-origin")]
    cors_origins: Vec<String>,

    /// Per-request timeout in seconds
    #[arg(long, default_value = "30")]
    request_timeout: u64,

    /// Maximum concurrent requests before load shedding
    #[arg(long, default_value = "1024")]
    max_concurrency: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let mut config = TileServerConfig::new(args.pmtiles_file)
        .bind_addr(args.bind)
        .cache_max_age(args.cache_max_age)
        .cors_allowed_origins(args.cors_origins)
        .request_timeout(Duration::from_secs(args.request_timeout))
        .max_concurrency(args.max_concurrency);
    if let Some(url) = args.public_url {
        config = config.public_url(url);
    }

    TileServer::open(config).await?.serve().await?;

    Ok(())
}
