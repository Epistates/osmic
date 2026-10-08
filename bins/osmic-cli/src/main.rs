//! `osmic` — OpenStreetMap to vector tiles, entity extraction and serving.

mod args;
mod cleanup;
mod cmd;

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

// The pipelines allocate heavily from every core; mimalloc's per-thread
// heaps avoid the contention of the platform allocators.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "osmic", version, about, long_about = None, propagate_version = true)]
struct Cli {
    /// More logging (-v debug, -vv trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Only log warnings and errors
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Log output format
    #[arg(long, value_enum, default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a PMTiles vector tile archive from OSM PBF (or GeoJSON)
    GenerateTiles(cmd::tiles::TilesArgs),
    /// Summarise a PBF file: element counts, features per layer, data quality
    Inspect(cmd::inspect::InspectArgs),
    /// Extract named entities (businesses, POIs) to CSV, JSON or GeoJSON
    Extract(cmd::extract::ExtractArgs),
    /// Serve a PMTiles archive over HTTP with a built-in map viewer
    Serve(cmd::serve::ServeArgs),
    /// Bring a PBF file up to date with an OSM replication server
    Update(cmd::update::UpdateArgs),
}

fn init_logging(cli: &Cli) {
    let default = match (cli.quiet, cli.verbose) {
        (true, _) => "warn",
        (false, 0) => "info",
        (false, 1) => "debug",
        (false, _) => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match cli.log_format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli);
    cleanup::install();
    let result = match cli.command {
        Command::GenerateTiles(a) => cmd::tiles::run(a),
        Command::Inspect(a) => cmd::inspect::run(a),
        Command::Extract(a) => cmd::extract::run(a),
        Command::Serve(a) => cmd::serve::run(a),
        Command::Update(a) => cmd::update::run(a),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            cleanup::remove_temporaries();
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
