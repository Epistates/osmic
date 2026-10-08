//! `osmic update`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Args;

use osmic_repl::{ClientOptions, UpdateOptions, update_pbf};

use super::{check_output, fmt_count};

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Sorted .osm.pbf file to bring up to date. Its header must carry the
    /// replication state (osmium, Geofabrik and osmic write it), or pass
    /// --server and --sequence.
    pub input: PathBuf,

    /// Write the updated file here instead of replacing the input
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Replication base URL (overrides the one in the PBF header), e.g.
    /// https://planet.openstreetmap.org/replication/minute/ or a Geofabrik
    /// region's …-updates/ directory
    #[arg(long)]
    pub server: Option<String>,

    /// Sequence the data is current to; overrides the PBF header's
    /// (required when --server is a different stream than the header's)
    #[arg(long)]
    pub sequence: Option<u64>,

    /// Maximum diffs to apply in one run
    #[arg(long, default_value_t = 1440)]
    pub max_diffs: u64,

    /// Maximum size of one downloaded diff (MiB)
    #[arg(long, default_value_t = 1024)]
    pub max_diff_mb: u64,

    /// Per-request timeout in seconds
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    /// Allow plain-HTTP replication servers (not recommended)
    #[arg(long)]
    pub allow_http: bool,

    /// Overwrite --output if it exists
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: UpdateArgs) -> anyhow::Result<()> {
    let start = Instant::now();
    if !args.input.is_file() {
        bail!("input file {} does not exist", args.input.display());
    }
    let output = match &args.output {
        Some(o) => {
            check_output(o, args.force)?;
            o.clone()
        }
        None => {
            // Also for a bare file name, whose temporary goes in ".".
            crate::cleanup::register_output(&args.input)?;
            args.input.clone()
        }
    };
    let options = UpdateOptions::default()
        .server(args.server.clone())
        .start_sequence(args.sequence)
        .max_diffs(args.max_diffs)
        .client(
            ClientOptions::default()
                .allow_http(args.allow_http)
                .request_timeout(Duration::from_secs(args.timeout))
                .max_diff_bytes(args.max_diff_mb.saturating_mul(1 << 20)),
        );
    let report = update_pbf(&args.input, &output, &options)
        .with_context(|| format!("updating {}", args.input.display()))?;

    println!(
        "sequence        {} -> {} (server at {})",
        report.from.sequence, report.to.sequence, report.latest_sequence
    );
    if let Some(t) = &report.to.timestamp {
        println!("data as of      {t}");
    }
    println!("diffs applied   {:>12}", fmt_count(report.diffs_applied));
    println!("created         {:>12}", fmt_count(report.stats.created));
    println!("modified        {:>12}", fmt_count(report.stats.modified));
    println!("deleted         {:>12}", fmt_count(report.stats.deleted));
    if report.diffs_applied > 0 {
        println!("output          {}", output.display());
    }
    if !report.up_to_date() {
        println!("not yet current: run again to continue");
    }
    println!("total time      {:>11.1}s", start.elapsed().as_secs_f64());
    Ok(())
}
