//! `osmic inspect`

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, bail};
use clap::Args;

use osmic_osm::{Feature, FeatureSink, NodeStorage, PbfProcessor, PipelineConfig, TagRetention};

use super::{fmt_bytes, fmt_count};
use crate::args::parse_node_store;

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Input .osm.pbf file
    pub input: PathBuf,

    /// Node location storage: sparse, dense[:MAX_ID] or file:PATH[:MAX_ID]
    #[arg(long, default_value = "sparse", value_parser = parse_node_store)]
    pub node_store: NodeStorage,
}

/// Counts features per layer without keeping them.
#[derive(Default)]
struct Counter(Mutex<BTreeMap<&'static str, u64>>);

impl FeatureSink for Counter {
    fn accept(
        &self,
        features: Vec<Feature>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut local: BTreeMap<&'static str, u64> = BTreeMap::new();
        for f in &features {
            *local.entry(f.kind.layer_name()).or_default() += 1;
        }
        let mut map = self.0.lock().map_err(|_| "counter lock poisoned")?;
        for (k, v) in local {
            *map.entry(k).or_default() += v;
        }
        Ok(())
    }
}

pub fn run(args: InspectArgs) -> anyhow::Result<()> {
    if !args.input.is_file() {
        bail!("input file {} does not exist", args.input.display());
    }
    let size = std::fs::metadata(&args.input)?.len();
    let processor = PbfProcessor::new(
        PipelineConfig::new()
            .node_storage(args.node_store)
            .tag_retention(TagRetention::Keys(Vec::<String>::new().into())),
    );
    let counter = Counter::default();
    let out = processor
        .run(&args.input, &counter)
        .with_context(|| format!("processing {}", args.input.display()))?;
    let h = &out.header;
    let s = &out.stats;

    println!("file              {}", args.input.display());
    println!("size              {}", fmt_bytes(size));
    if let Some(p) = &h.writing_program {
        println!("writing program   {p}");
    }
    println!("sorted            {}", h.is_sorted());
    println!("locations on ways {}", h.has_locations_on_ways());
    if let Some(seq) = h.replication_sequence {
        println!("replication seq   {seq}");
    }
    if let Some(b) = h.bbox {
        println!("header bbox       {b}");
    }
    println!();
    println!("nodes             {:>14}", fmt_count(s.node_count));
    println!("ways              {:>14}", fmt_count(s.way_count));
    println!("relations         {:>14}", fmt_count(s.relation_count));
    println!("features          {:>14}", fmt_count(s.feature_count));
    println!("incomplete ways   {:>14}", fmt_count(s.incomplete_ways));
    println!(
        "area relations    {:>14}  (assembled {}, incomplete {}, invalid {})",
        fmt_count(s.area_relations),
        fmt_count(s.assembled_relations),
        fmt_count(s.incomplete_relations),
        fmt_count(s.invalid_relations)
    );
    println!("role mismatches   {:>14}", fmt_count(s.role_mismatches));
    println!("invalid nodes     {:>14}", fmt_count(s.invalid_nodes));
    println!("node index        {:>14}", fmt_bytes(s.node_index_bytes));
    println!("data bbox         {}", out.bbox);
    println!(
        "time              {:>13.1}s  (pass 1 {:.1}s, pass 2 {:.1}s)",
        s.total_duration.as_secs_f64(),
        s.pass1_duration.as_secs_f64(),
        s.pass2_duration.as_secs_f64()
    );
    println!();
    let counts = counter.0.into_inner().unwrap_or_else(|p| p.into_inner());
    let mut by_count: Vec<_> = counts.into_iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (layer, n) in by_count {
        println!("  {layer:<14} {:>14}", fmt_count(n));
    }
    Ok(())
}
