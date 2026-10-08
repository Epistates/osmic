//! Run the PBF pipeline over a file and print statistics without keeping
//! features in memory.
//!
//! ```sh
//! cargo run --release -p osmic-osm --example pbf_stats -- extract.osm.pbf [sparse|dense:<max_node_id>]
//! ```

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use osmic_osm::{Feature, FeatureSink, NodeStorage, PbfProcessor, PipelineConfig};

#[derive(Default)]
struct CountingSink {
    total: AtomicU64,
    by_layer: Mutex<BTreeMap<&'static str, u64>>,
}

impl FeatureSink for CountingSink {
    fn accept(
        &self,
        features: Vec<Feature>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.total
            .fetch_add(features.len() as u64, Ordering::Relaxed);
        let mut local: BTreeMap<&'static str, u64> = BTreeMap::new();
        for f in &features {
            *local.entry(f.kind.layer_name()).or_default() += 1;
        }
        let mut map = self.by_layer.lock().map_err(|_| "poisoned")?;
        for (k, v) in local {
            *map.entry(k).or_default() += v;
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber_init();
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: pbf_stats <file.osm.pbf> [sparse|dense:<max_id>]")?;
    let node_storage = match args.next().as_deref() {
        None | Some("sparse") => NodeStorage::Sparse,
        Some(s) => match s.strip_prefix("dense:") {
            Some(max) => NodeStorage::DenseMemory {
                max_node_id: max.parse()?,
            },
            None => return Err(format!("unknown storage {s:?}").into()),
        },
    };
    let processor = PbfProcessor::new(PipelineConfig {
        node_storage,
        ..Default::default()
    });
    let sink = CountingSink::default();
    let out = processor.run(path.as_ref(), &sink)?;
    let s = &out.stats;
    println!("nodes                {:>14}", s.node_count);
    println!("ways                 {:>14}", s.way_count);
    println!("relations            {:>14}", s.relation_count);
    println!(
        "features             {:>14}",
        sink.total.load(Ordering::Relaxed)
    );
    println!("incomplete ways      {:>14}", s.incomplete_ways);
    println!("area relations       {:>14}", s.area_relations);
    println!("  assembled          {:>14}", s.assembled_relations);
    println!("  incomplete         {:>14}", s.incomplete_relations);
    println!("  invalid            {:>14}", s.invalid_relations);
    println!("role mismatches      {:>14}", s.role_mismatches);
    println!("node index           {:>11} MiB", s.node_index_bytes >> 20);
    println!("interned strings     {:>14}", processor.tag_store().len());
    println!(
        "pass 1               {:>13.1}s",
        s.pass1_duration.as_secs_f64()
    );
    println!(
        "pass 2 + relations   {:>13.1}s",
        s.pass2_duration.as_secs_f64()
    );
    println!("bbox                 {}", out.bbox);
    for (layer, n) in sink.by_layer.lock().map_err(|_| "poisoned")?.iter() {
        println!("  {layer:<12} {n:>14}");
    }
    Ok(())
}

fn tracing_subscriber_init() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
