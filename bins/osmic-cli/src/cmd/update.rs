//! `osmic update`

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;

use osmic_index::DenseNodeStore;
use osmic_osm::{LayerSet, TagStore};

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// State directory for replication tracking
    #[arg(long)]
    pub state_dir: PathBuf,

    /// Replication base URL
    #[arg(
        long,
        default_value = "https://planet.openstreetmap.org/replication/minute/"
    )]
    pub replication_url: String,

    /// Feature store database
    #[arg(long, default_value = "osmic-features.redb")]
    pub feature_store: PathBuf,

    /// Persistent node store written by `generate-tiles --node-store file:PATH`
    #[arg(long)]
    pub node_store: PathBuf,

    /// Initialise replication at this sequence number (refuses to replace
    /// existing state)
    #[arg(long)]
    pub init_sequence: Option<u64>,
}

pub fn run(args: UpdateArgs) -> anyhow::Result<()> {
    let mut state = match args.init_sequence {
        Some(seq) => {
            if args.state_dir.join("state.json").exists() {
                bail!(
                    "replication state already exists in {}; refusing to reinitialise",
                    args.state_dir.display()
                );
            }
            let s = osmic_repl::ReplicationState::init(&args.replication_url, seq);
            s.save(&args.state_dir)?;
            s
        }
        None => osmic_repl::ReplicationState::load(&args.state_dir)
            .with_context(|| format!("loading state from {}", args.state_dir.display()))?,
    };
    let node_store = DenseNodeStore::open(&args.node_store)
        .with_context(|| format!("opening node store {}", args.node_store.display()))?;
    let store = osmic_repl::FeatureStore::open(&args.feature_store)?;
    let tag_store = TagStore::new();
    let config = osmic_tiles::TileGeneratorConfig::default();

    let url = state.next_osc_url();
    eprintln!("Downloading {url}");
    let response = reqwest::blocking::get(&url)?;
    if !response.status().is_success() {
        bail!(
            "replication server returned {} for {url}",
            response.status()
        );
    }
    let changes = osmic_repl::osc::parse_osc_gz_bytes(&response.bytes()?)?;
    let dirty = osmic_repl::apply_changes(
        &changes,
        &store,
        &node_store,
        &tag_store,
        &LayerSet::all(),
        &config,
    )?;
    state.sequence_number += 1;
    state.save(&args.state_dir)?;
    println!("changes {}  dirty tiles {}", changes.len(), dirty.len());
    Ok(())
}
