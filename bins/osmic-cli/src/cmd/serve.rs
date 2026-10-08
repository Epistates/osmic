//! `osmic serve`

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;

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
}

pub fn run(args: ServeArgs) -> anyhow::Result<()> {
    if !args.pmtiles.is_file() {
        bail!("{} does not exist", args.pmtiles.display());
    }
    let config = osmic_serve::TileServerConfig {
        bind_addr: args.bind,
        pmtiles_path: args.pmtiles.clone(),
        cache_max_age: args.cache_max_age,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(osmic_serve::TileServer::new(config).serve())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("serving {}", args.pmtiles.display()))
}
